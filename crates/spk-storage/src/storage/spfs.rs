// Copyright (c) Sony Pictures Imageworks, et al.
// SPDX-License-Identifier: Apache-2.0
// https://github.com/imageworks/spk
use std::collections::{hash_map, HashMap, HashSet};
use std::convert::{TryFrom, TryInto};
use std::str::FromStr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use futures::StreamExt;
use itertools::Itertools;
use once_cell::sync::Lazy;
use relative_path::RelativePathBuf;
use serde::{Deserialize, Serialize};
use spfs::prelude::*;
use spfs::storage::{EntryType, Repository};
use spfs::tracking;
use spk_schema::foundation::ident_build::{parse_build, Build};
use spk_schema::foundation::ident_component::Component;
use spk_schema::foundation::name::{PkgName, PkgNameBuf, RepositoryName, RepositoryNameBuf};
use spk_schema::foundation::version::{parse_version, Version};
use spk_schema::ident::VersionIdent;
use spk_schema::ident_build::parsing::embedded_source_package;
use spk_schema::ident_build::{EmbeddedSource, EmbeddedSourcePackage};
use spk_schema::ident_ops::TagPath;
use spk_schema::version::VersionParts;
use spk_schema::{AnyIdent, BuildIdent, FromYaml, Package, Recipe, Spec, SpecRecipe};
use tokio::io::AsyncReadExt;

use super::repository::{PublishPolicy, Storage};
use super::CachePolicy;
use crate::storage::repository::internal::RepositoryExt;
use crate::{with_cache_policy, Error, Result};

#[cfg(test)]
#[path = "./spfs_test.rs"]
mod spfs_test;

const REPO_METADATA_TAG: &str = "spk/repo";
const REPO_VERSION: &str = "1.0.0";

#[derive(Debug)]
pub struct SpfsRepository {
    address: url::Url,
    name: RepositoryNameBuf,
    inner: spfs::storage::RepositoryHandle,
    cache_policy: AtomicPtr<CachePolicy>,
    caches: CachesForAddress,
}

impl std::hash::Hash for SpfsRepository {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.address.hash(state);
    }
}

impl Ord for SpfsRepository {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.address.cmp(&other.address)
    }
}

impl PartialOrd for SpfsRepository {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for SpfsRepository {
    fn eq(&self, other: &Self) -> bool {
        self.address == other.address
    }
}

impl Eq for SpfsRepository {}

impl std::ops::Deref for SpfsRepository {
    type Target = spfs::storage::RepositoryHandle;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::ops::DerefMut for SpfsRepository {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<S: AsRef<str>, T: Into<spfs::storage::RepositoryHandle>> TryFrom<(S, T)> for SpfsRepository {
    type Error = crate::Error;

    fn try_from(name_and_repo: (S, T)) -> Result<Self> {
        let inner = name_and_repo.1.into();
        let address = inner.address();
        Ok(Self {
            caches: CachesForAddress::new(&address),
            address,
            name: name_and_repo.0.as_ref().try_into()?,
            inner,
            cache_policy: AtomicPtr::new(Box::leak(Box::new(CachePolicy::CacheOk))),
        })
    }
}

impl SpfsRepository {
    pub async fn new(name: &str, address: &str) -> Result<Self> {
        let inner = spfs::open_repository(address).await?;
        let address = inner.address();
        Ok(Self {
            caches: CachesForAddress::new(&address),
            address,
            name: name.try_into()?,
            inner,
            cache_policy: AtomicPtr::new(Box::leak(Box::new(CachePolicy::CacheOk))),
        })
    }

    /// Pin this repository to a specific point in time, limiting
    /// all queries and making it read-only
    pub fn pin_at_time(&mut self, ts: &spfs::tracking::TimeSpec) {
        // Safety: we are going to mutate and replace the value that
        // is being read here, and know that self.inner is both
        // initialized and valid for reads
        let tmp = unsafe { std::ptr::read(&self.inner) };
        let new = tmp.into_pinned(ts.to_datetime_from_now());
        // Safety: we are replacing the old value with a moved copy
        // of itself, and so explicitly do not want the old value
        // dropped or accessed in any way
        unsafe { std::ptr::write(&mut self.inner, new) };
        self.address
            .query_pairs_mut()
            .append_pair("when", &ts.to_string());
    }
}

impl std::ops::Drop for SpfsRepository {
    fn drop(&mut self) {
        // Safety: We only put valid `Box` pointers into `self.cache_policy`.
        unsafe {
            let _ = Box::from_raw(self.cache_policy.load(Ordering::Relaxed));
        }
    }
}

#[derive(Clone)]
enum CacheValue<T> {
    InvalidPackageSpec(AnyIdent, String),
    PackageNotFound(AnyIdent),
    StringError(String),
    StringifiedError(String),
    Success(T),
}

impl<T> From<CacheValue<T>> for Result<T> {
    fn from(cv: CacheValue<T>) -> Self {
        match cv {
            CacheValue::InvalidPackageSpec(i, err) => Err(crate::Error::InvalidPackageSpec(i, err)),
            CacheValue::PackageNotFound(i) => Err(Error::PackageNotFound(i)),
            CacheValue::StringError(s) => Err(s.into()),
            CacheValue::StringifiedError(s) => Err(s.into()),
            CacheValue::Success(v) => Ok(v),
        }
    }
}

impl<T> From<std::result::Result<T, &crate::Error>> for CacheValue<T> {
    fn from(r: std::result::Result<T, &crate::Error>) -> Self {
        match r {
            Ok(v) => CacheValue::Success(v),
            Err(crate::Error::InvalidPackageSpec(i, err)) => {
                CacheValue::InvalidPackageSpec(i.clone(), err.to_string())
            }
            Err(Error::PackageNotFound(i)) => CacheValue::PackageNotFound(i.clone()),
            Err(crate::Error::String(s)) => CacheValue::StringError(s.clone()),
            // Decorate the error message so we can tell it was a custom error
            // downgraded to a String.
            Err(err) => CacheValue::StringifiedError(format!("Cached error: {err}")),
        }
    }
}

// To keep clippy happy
type ArcVecArcVersion = Arc<Vec<Arc<Version>>>;
/// The set of caches for a specific repository.
#[derive(Clone)]
struct CachesForAddress {
    /// Components list cache for list_build_components()
    list_build_components: Arc<DashMap<BuildIdent, CacheValue<Vec<Component>>>>,
    /// EntryTypes list cache for ls_tags() caches
    ls_tags: Arc<DashMap<relative_path::RelativePathBuf, Vec<EntryType>>>,
    /// Package specs cache for read_component_from_storage() and read_embed_stub()
    package: Arc<DashMap<BuildIdent, CacheValue<Arc<Spec>>>>,
    /// Versions list cache for list_packages_versions()
    package_versions: Arc<DashMap<PkgNameBuf, CacheValue<ArcVecArcVersion>>>,
    /// Recipe specs cache for read_recipe()
    recipe: Arc<DashMap<VersionIdent, CacheValue<Arc<spk_schema::SpecRecipe>>>>,
    /// Recipe specs cache for read_recipe()
    tag_spec: Arc<DashMap<tracking::TagSpec, CacheValue<tracking::Tag>>>,
}

static CACHES_FOR_ADDRESS: Lazy<std::sync::Mutex<HashMap<String, CachesForAddress>>> =
    Lazy::new(|| std::sync::Mutex::new(HashMap::new()));

impl CachesForAddress {
    fn new(address: &url::Url) -> Self {
        let mut caches = CACHES_FOR_ADDRESS.lock().unwrap();
        match caches.entry(address.as_str().to_owned()) {
            hash_map::Entry::Occupied(entry) => entry.get().clone(),
            hash_map::Entry::Vacant(entry) => entry
                .insert(Self {
                    list_build_components: Arc::new(DashMap::new()),
                    ls_tags: Arc::new(DashMap::new()),
                    package: Arc::new(DashMap::new()),
                    package_versions: Arc::new(DashMap::new()),
                    recipe: Arc::new(DashMap::new()),
                    tag_spec: Arc::new(DashMap::new()),
                })
                .clone(),
        }
    }
}

impl std::fmt::Debug for CachesForAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachesForAddress").finish()
    }
}

#[async_trait::async_trait]
impl Storage for SpfsRepository {
    type Recipe = SpecRecipe;
    type Package = Spec;

    async fn get_concrete_package_builds(&self, pkg: &VersionIdent) -> Result<HashSet<BuildIdent>> {
        // It is possible for a `spk/spec/pkgname/1.0.0/BUILDKEY` tag to
        // exist without a corresponding `spk/spk/pkgname/1.0.0/BUILDKEY`
        // tag. In this scenario, "pkgname" will appear in the results of
        // `list_packages` and `list_package_versions`, because those look at
        // the `spk/spec/...` spfs tag tree, i.e., this package will appear
        // in the output of `spk ls`. In order to make it possible to locate
        // the build spec, e.g., for `spk rm pkgname/1.0.0` to work, this
        // method needs to return a union of all the build tags of both the
        // `spk/spec/` and `spk/pkg/` tag trees.

        let mut builds = HashSet::new();

        // The repo may contain tags with different numbers of parts in the
        // version, but we treat different amounts of trailing zeros as equal,
        // e.g., 1.0 == 1.0.0. So first we normalize the provided version to
        // remove any trailing zeros, but then we look in the repo for various
        // lengths of trailing zeros. This is capped at 5 to handle all known
        // existing packages (at SPI).
        //
        // Example:
        //
        //     `pkg` == "pkgname/1.2.0"
        //
        //     `normalized_parts` == [1, 2]
        //
        //     Check the following tag paths:
        //         - spk/{spec,pkg}/pkgname/1.2
        //         - spk/{spec,pkg}/pkgname/1.2.0
        //         - spk/{spec,pkg}/pkgname/1.2.0.0
        //         - spk/{spec,pkg}/pkgname/1.2.0.0.0
        let normalized_parts = pkg.version().parts.normalize();
        for num_parts in (1..=5)
            // Handle all the part lengths that are bigger than the normalized
            // parts, except for the normalized parts length itself, which may
            // be larger than 5 and not hit by this range.
            .filter(|num_parts| *num_parts > normalized_parts.len())
            // Then, handle the normalized parts length itself, which is
            // skipped by the filter above so it isn't processed twice,
            // and is handled even if the length is outside the above range.
            .chain(std::iter::once(normalized_parts.len()))
        {
            let new_parts = normalized_parts
                .iter()
                .chain(std::iter::repeat(&0))
                .take(num_parts)
                .copied()
                .collect::<Vec<_>>();

            let pkg = pkg.with_version(Version {
                parts: VersionParts {
                    parts: new_parts,
                    plus_epsilon: normalized_parts.plus_epsilon,
                },
                pre: pkg.version().pre.clone(),
                post: pkg.version().post.clone(),
            });

            let spec_base = self.build_spec_tag(&pkg);
            let package_base = self.build_package_tag(&pkg);

            let spec_tags = self.ls_tags(&spec_base);
            let package_tags = self.ls_tags(&package_base);

            let (spec_tags, package_tags) = tokio::join!(spec_tags, package_tags);

            builds.extend(
                spec_tags
                    .into_iter()
                    .chain(package_tags)
                    .filter_map(|entry| match entry {
                        Ok(EntryType::Tag(name))
                            if !name.starts_with(EmbeddedSourcePackage::EMBEDDED_BY_PREFIX) =>
                        {
                            Some(name)
                        }
                        Ok(EntryType::Tag(_)) => None,
                        Ok(EntryType::Folder(name)) => Some(name),
                        Err(_) => None,
                    })
                    .filter_map(|b| match parse_build(&b) {
                        Ok(v) => Some(v),
                        Err(_) => {
                            tracing::warn!("Invalid build found in spfs tags: {}", b);
                            None
                        }
                    })
                    .map(|b| pkg.to_build(b)),
            );
        }

        Ok(builds)
    }

    async fn get_embedded_package_builds(&self, pkg: &VersionIdent) -> Result<HashSet<BuildIdent>> {
        let pkg = pkg.to_any(Some(Build::Source));
        let mut base = self.build_spec_tag(&pkg);
        // the package tag contains the name and build, but we need to
        // remove the trailing build in order to list the containing 'folder'
        // eg: pkg/1.0.0/src => pkg/1.0.0
        base.pop();

        let builds: HashSet<_> = self
            .ls_tags(&base)
            .await
            .into_iter()
            .filter_map(|entry| match entry {
                Ok(EntryType::Tag(name)) => Some(name),
                Ok(EntryType::Folder(_)) => None,
                Err(_) => None,
            })
            .filter_map(|b| {
                b.strip_prefix(EmbeddedSourcePackage::EMBEDDED_BY_PREFIX)
                    .and_then(|encoded_ident| {
                        data_encoding::BASE32_NOPAD
                            .decode(encoded_ident.as_bytes())
                            .ok()
                    })
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .and_then(|ident_str| {
                        // The decoded BASE32 value will look something like this:
                        //
                        //     "embedded[embed-projection:run/1.0/3I42H3S6]"
                        //
                        // The `embedded_source_package` parser knows how to
                        // parse the "[...]" part and return the type we want,
                        // but we need to strip the "embedded" prefix.
                        ident_str
                            .strip_prefix("embedded")
                            .and_then(|ident_str| {
                                use nom::combinator::all_consuming;

                                all_consuming(
                                        embedded_source_package::<(_, nom::error::ErrorKind)>,
                                    )(ident_str)
                                    .map(|(_, ident_with_components)| ident_with_components)
                                    .ok()
                            })
                            .map(Build::Embedded)
                    })
            })
            .map(|b| pkg.to_build(b))
            .collect();

        Ok(builds)
    }

    async fn publish_embed_stub_to_storage(&self, spec: &Self::Package) -> Result<()> {
        let ident = spec.ident();
        let tag_path = self.build_spec_tag(ident);
        let tag_spec = spfs::tracking::TagSpec::parse(tag_path.as_str())?;

        let payload = serde_yaml::to_string(&spec)
            .map_err(|err| Error::SpkSpecError(spk_schema::Error::SpecEncodingError(err)))?;
        let digest = self
            .inner
            .commit_blob(Box::pin(std::io::Cursor::new(payload.into_bytes())))
            .await?;
        self.inner.push_tag(&tag_spec, &digest).await?;
        self.invalidate_caches();
        Ok(())
    }

    async fn publish_package_to_storage(
        &self,
        package: &<Self::Recipe as spk_schema::Recipe>::Output,
        components: &HashMap<Component, spfs::encoding::Digest>,
    ) -> Result<()> {
        let tag_path = self.build_package_tag(package.ident());

        // We will also publish the 'run' component in the old style
        // for compatibility with older versions of the spk command.
        // It's not perfect but at least the package will be visible
        let legacy_tag = spfs::tracking::TagSpec::parse(&tag_path)?;
        let legacy_component = if package.ident().is_source() {
            *components.get(&Component::Source).ok_or_else(|| {
                Error::String("Package must have a source component to be published".to_string())
            })?
        } else {
            *components.get(&Component::Run).ok_or_else(|| {
                Error::String("Package must have a run component to be published".to_string())
            })?
        };

        self.inner.push_tag(&legacy_tag, &legacy_component).await?;

        let components: std::result::Result<Vec<_>, _> = components
            .iter()
            .map(|(name, digest)| {
                spfs::tracking::TagSpec::parse(tag_path.join(name.as_str()))
                    .map(|spec| (spec, digest))
            })
            .collect();
        for (tag_spec, digest) in components?.into_iter() {
            self.inner.push_tag(&tag_spec, digest).await?;
        }

        // TODO: dedupe this part with force_publish_recipe
        let tag_path = self.build_spec_tag(package.ident());
        let tag_spec = spfs::tracking::TagSpec::parse(tag_path)?;
        let payload = serde_yaml::to_string(&package)
            .map_err(|err| Error::SpkSpecError(spk_schema::Error::SpecEncodingError(err)))?;
        let digest = self
            .inner
            .commit_blob(Box::pin(std::io::Cursor::new(payload.into_bytes())))
            .await?;
        self.inner.push_tag(&tag_spec, &digest).await?;
        self.invalidate_caches();
        Ok(())
    }

    async fn publish_recipe_to_storage(
        &self,
        spec: &Self::Recipe,
        publish_policy: PublishPolicy,
    ) -> Result<()> {
        let ident = spec.ident();
        let tag_path = self.build_spec_tag(ident);
        let tag_spec = spfs::tracking::TagSpec::parse(tag_path.as_str())?;
        if matches!(publish_policy, PublishPolicy::DoNotOverwriteVersion)
            && self.inner.has_tag(&tag_spec).await
        {
            // BUG(rbottriell): this creates a race condition but is not super dangerous
            // because of the non-destructive tag history
            return Err(Error::VersionExists(ident.clone()));
        }

        let payload = serde_yaml::to_string(&spec)
            .map_err(|err| Error::SpkSpecError(spk_schema::Error::SpecEncodingError(err)))?;
        let digest = self
            .inner
            .commit_blob(Box::pin(std::io::Cursor::new(payload.into_bytes())))
            .await?;
        self.inner.push_tag(&tag_spec, &digest).await?;
        self.invalidate_caches();
        Ok(())
    }

    async fn read_components_from_storage(
        &self,
        pkg: &BuildIdent,
    ) -> Result<HashMap<Component, spfs::encoding::Digest>> {
        if pkg.build().is_embedded() {
            return Ok(HashMap::new());
        }
        let package = self.lookup_package(pkg).await?;
        let component_tags = package.into_components();
        let mut components = HashMap::with_capacity(component_tags.len());
        for (name, tag_spec) in component_tags.into_iter() {
            let tag = self.resolve_tag(|| pkg.to_any(), &tag_spec).await?;
            components.insert(name, tag.target);
        }
        Ok(components)
    }

    async fn read_package_from_storage(
        &self,
        pkg: &BuildIdent,
    ) -> Result<Arc<<Self::Recipe as spk_schema::Recipe>::Output>> {
        // TODO: reduce duplicate code with read_recipe
        if self.cached_result_permitted() {
            if let Some(v) = self.caches.package.get(pkg) {
                return v.value().clone().into();
            }
        }

        let r: Result<Arc<Spec>> = async {
            let tag_path = self.build_spec_tag(pkg);
            let tag_spec = spfs::tracking::TagSpec::parse(tag_path.as_str())?;
            let tag = self.resolve_tag(|| pkg.to_any(), &tag_spec).await?;

            let (mut reader, filename) = self.inner.open_payload(tag.target).await?;
            let mut yaml = String::new();
            reader
                .read_to_string(&mut yaml)
                .await
                .map_err(|err| Error::FileReadError(filename, err))?;
            Spec::from_yaml(&yaml)
                .map_err(|err| Error::InvalidPackageSpec(pkg.to_any(), err.to_string()))
                .map(Arc::new)
        }
        .await;

        self.caches
            .package
            .insert(pkg.clone(), r.as_ref().map(Arc::clone).into());
        r
    }

    async fn remove_embed_stub_from_storage(&self, pkg: &BuildIdent) -> Result<()> {
        let tag_path = self.build_spec_tag(pkg);
        let tag_spec = spfs::tracking::TagSpec::parse(&tag_path)?;
        match self.inner.remove_tag_stream(&tag_spec).await {
            Err(spfs::Error::UnknownReference(_)) => Err(Error::PackageNotFound(pkg.to_any())),
            Err(err) => Err(err.into()),
            Ok(_) => {
                self.invalidate_caches();
                Ok(())
            }
        }
    }

    async fn remove_package_from_storage(&self, pkg: &BuildIdent) -> Result<()> {
        // The three things this method is responsible for deleting are:
        //
        // 1. Component build tags like: `spk/pkg/example/4.2.1/GMTG3CXY/build`.
        // 2. Legacy build tags like   : `spk/pkg/example/4.2.1/GMTG3CXY`.
        // 3. Build recipe tags like   : `spk/spec/example/4.2.1/GMTG3CXY`.
        //
        // It should make an effort to delete all three types before returning
        // any failures.

        let component_tags = async {
            let mut deleted_something = false;

            for tag_spec in
                with_cache_policy!(self, CachePolicy::BypassCache, { self.lookup_package(pkg) })
                    .await?
                    .tags()
            {
                match self.inner.remove_tag_stream(tag_spec).await {
                    Err(spfs::Error::UnknownReference(_)) => (),
                    Ok(_) => deleted_something = true,
                    res => res?,
                };
            }
            Ok::<_, Error>(deleted_something)
        };

        let legacy_tags = async {
            // because we double-publish packages to be visible/compatible
            // with the old repo tag structure, we must also try to remove
            // the legacy version of the tag after removing the discovered
            // as it may still be there and cause the removal to be ineffective
            let mut deleted_something = false;

            if let Ok(legacy_tag) = spfs::tracking::TagSpec::parse(self.build_package_tag(pkg)) {
                match self.inner.remove_tag_stream(&legacy_tag).await {
                    Err(spfs::Error::UnknownReference(_)) => (),
                    Ok(_) => deleted_something = true,
                    res => res?,
                }
            };
            Ok::<_, Error>(deleted_something)
        };

        let build_recipe_tags = async {
            let tag_path = self.build_spec_tag(pkg);
            let tag_spec = spfs::tracking::TagSpec::parse(&tag_path)?;
            match self.inner.remove_tag_stream(&tag_spec).await {
                Err(spfs::Error::UnknownReference(_)) => Err(Error::PackageNotFound(pkg.to_any())),
                Err(err) => Err(err.into()),
                Ok(_) => Ok(true),
            }
        };

        let (component_tags_result, legacy_tags_result, build_recipe_tags_result) =
            tokio::join!(component_tags, legacy_tags, build_recipe_tags);

        // Still invalidate caches in case some of individual deletions were
        // successful.
        self.invalidate_caches();

        // If any of the three sub-tasks successfully deleted something *and*
        // the only failures otherwise was `PackageNotFound`, then return
        // success. Since something was deleted then the package was
        // technically "found."
        //
        // Allow manual_try_fold since this logic can't short-circuit all errors.
        #[allow(clippy::manual_try_fold)]
        [
            component_tags_result,
            build_recipe_tags_result,
            // Check legacy tags last because errors deleting legacy tags are
            // less important.
            legacy_tags_result,
        ]
        .into_iter()
        .fold(Ok::<_, Error>(false), |acc, x| match (acc, x) {
            // Preserve the first non-PackageNotFound encountered.
            (Err(err), _) if !err.is_package_not_found() => Err(err),
            // Incoming error is not PackageNotFound.
            (_, Err(err)) if !err.is_package_not_found() => Err(err),
            // Successes merge with successes and retain "deleted
            // something" if either did.
            (Ok(x), Ok(y)) => Ok(x || y),
            // Having successfully deleted something trumps
            // `PackageNotFound`.
            (Ok(true), Err(err)) if err.is_package_not_found() => Ok(true),
            (Err(err), Ok(true)) if err.is_package_not_found() => Ok(true),
            // Otherwise, keep the prevailing error.
            (Err(err), _) => Err(err),
            (_, Err(err)) => Err(err),
        })
        .and_then(|deleted_something| {
            if deleted_something {
                Ok(())
            } else {
                Err(Error::PackageNotFound(pkg.to_any()))
            }
        })
    }
}

#[async_trait::async_trait]
impl crate::Repository for SpfsRepository {
    fn address(&self) -> &url::Url {
        &self.address
    }

    async fn list_packages(&self) -> Result<Vec<PkgNameBuf>> {
        let path = relative_path::RelativePath::new("spk/spec");
        // XXX: infallible vs return type
        Ok(self
            .ls_tags(path)
            .await
            .into_iter()
            .filter_map(|entry| match entry {
                Ok(EntryType::Folder(name)) => name.parse().ok(),
                Ok(EntryType::Tag(_)) => None,
                Err(_) => None,
            })
            .collect::<Vec<_>>())
    }

    async fn list_package_versions(&self, name: &PkgName) -> Result<Arc<Vec<Arc<Version>>>> {
        if self.cached_result_permitted() {
            if let Some(v) = self.caches.package_versions.get(name) {
                return v.value().clone().into();
            }
        }
        let r: Result<Arc<_>> = async {
            let path = self.build_spec_tag(&VersionIdent::new_zero(name).into_any(None));
            let versions: HashSet<_> = self
                .ls_tags(&path)
                .await
                .into_iter()
                .filter_map(|entry| match entry {
                    // undo our encoding of the invalid '+' character in spfs tags
                    Ok(EntryType::Folder(name)) => Some(name.replace("..", "+")),
                    Ok(EntryType::Tag(name)) => Some(name.replace("..", "+")),
                    Err(_) => None,
                })
                .filter_map(|v| match parse_version(&v) {
                    Ok(v) => Some(v),
                    Err(_) => {
                        tracing::warn!("Invalid version found in spfs tags: {}", v);
                        None
                    }
                })
                .collect();
            let mut versions = versions.into_iter().map(Arc::new).collect_vec();
            versions.sort();
            // XXX: infallible vs return type
            Ok(Arc::new(versions))
        }
        .await;

        self.caches
            .package_versions
            .insert(name.to_owned(), r.as_ref().map(|b| b.clone()).into());
        r
    }

    async fn list_build_components(&self, pkg: &BuildIdent) -> Result<Vec<Component>> {
        if self.cached_result_permitted() {
            if let Some(v) = self.caches.list_build_components.get(pkg) {
                return v.value().clone().into();
            }
        }

        let r = if pkg.build().is_embedded() {
            Ok(Vec::new())
        } else {
            match self.lookup_package(pkg).await {
                Ok(p) => Ok(p.into_components().into_keys().collect()),
                Err(Error::PackageNotFound(_)) => Ok(Vec::new()),
                Err(err) => Err(err),
            }
        };

        self.caches
            .list_build_components
            .insert(pkg.to_owned(), r.as_ref().map(|v| v.clone()).into());
        r
    }

    fn name(&self) -> &RepositoryName {
        &self.name
    }

    async fn read_embed_stub(&self, pkg: &BuildIdent) -> Result<Arc<Self::Package>> {
        // This is similar to read_recipe but it returns a package and
        // uses the package cache.
        match pkg.build() {
            Build::Embedded(EmbeddedSource::Package { .. }) => {
                // Allow embedded stubs to be read as a "package"
            }
            _ => {
                return Err(format!("Cannot read this ident as an embed stub: {pkg}").into());
            }
        };
        if self.cached_result_permitted() {
            if let Some(v) = self.caches.package.get(pkg) {
                return v.value().clone().into();
            }
        }
        let r: Result<Arc<Spec>> = async {
            let tag_path = self.build_spec_tag(pkg);
            let tag_spec = spfs::tracking::TagSpec::parse(tag_path.as_str())?;
            let tag = self.resolve_tag(|| pkg.to_any(), &tag_spec).await?;

            let (mut reader, _) = self.inner.open_payload(tag.target).await?;
            let mut yaml = String::new();
            reader
                .read_to_string(&mut yaml)
                .await
                .map_err(|err| Error::FileReadError(tag.target.to_string().into(), err))?;
            Spec::from_yaml(yaml)
                .map_err(|err| Error::InvalidPackageSpec(pkg.to_any(), err.to_string()))
                .map(Arc::new)
        }
        .await;

        self.caches
            .package
            .insert(pkg.clone(), r.as_ref().map(Arc::clone).into());
        r
    }

    async fn read_recipe(&self, pkg: &VersionIdent) -> Result<Arc<Self::Recipe>> {
        if self.cached_result_permitted() {
            if let Some(v) = self.caches.recipe.get(pkg) {
                return v.value().clone().into();
            }
        }
        let r: Result<Arc<SpecRecipe>> = async {
            let tag_path = self.build_spec_tag(pkg);
            let tag_spec = spfs::tracking::TagSpec::parse(tag_path.as_str())?;
            let tag = self.resolve_tag(|| pkg.to_any(None), &tag_spec).await?;

            let (mut reader, _) = self.inner.open_payload(tag.target).await?;
            let mut yaml = String::new();
            reader
                .read_to_string(&mut yaml)
                .await
                .map_err(|err| Error::FileReadError(tag.target.to_string().into(), err))?;
            SpecRecipe::from_yaml(yaml)
                .map_err(|err| Error::InvalidPackageSpec(pkg.to_any(None), err.to_string()))
                .map(Arc::new)
        }
        .await;

        self.caches
            .recipe
            .insert(pkg.clone(), r.as_ref().map(Arc::clone).into());
        r
    }

    async fn remove_recipe(&self, pkg: &VersionIdent) -> Result<()> {
        let tag_path = self.build_spec_tag(pkg);
        let tag_spec = spfs::tracking::TagSpec::parse(&tag_path)?;
        match self.inner.remove_tag_stream(&tag_spec).await {
            Err(spfs::Error::UnknownReference(_)) => Err(Error::PackageNotFound(pkg.to_any(None))),
            Err(err) => Err(err.into()),
            Ok(_) => {
                self.invalidate_caches();
                Ok(())
            }
        }
    }

    async fn upgrade(&self) -> Result<String> {
        let target_version = Version::from_str(REPO_VERSION).unwrap();
        let mut meta = self.read_metadata().await?;
        if meta.version > target_version {
            // for this particular upgrade (moving old-style tags to new)
            // we allow it to be run again over the same repo since it's
            // possible that some clients are still publishing the old way
            // during the transition period
            return Ok("Nothing to do.".to_string());
        }
        for name in self.list_packages().await? {
            tracing::info!("Processing {name}...");
            let mut pkg = VersionIdent::new_zero(&*name).into_any(None);
            for version in self.list_package_versions(&name).await?.iter() {
                pkg.set_version((**version).clone());
                for build in self.list_package_builds(pkg.as_version()).await? {
                    if build.is_embedded() {
                        // XXX `lookup_package` isn't able to read embed stubs.
                        // Should it be able to?
                        continue;
                    }
                    let stored = with_cache_policy!(self, CachePolicy::BypassCache, {
                        self.lookup_package(&build)
                    })
                    .await?;

                    // [Re-]create embedded stubs.
                    if build.can_embed() {
                        let spec = self.read_package(&build).await?;
                        let providers = self.get_embedded_providers(&spec)?;
                        if !providers.is_empty() {
                            tracing::info!("Creating embedded stubs for {name}...");
                            for (embedded, components) in providers.into_iter() {
                                self.create_embedded_stub_for_spec(&spec, &embedded, components)
                                    .await?
                            }
                        }
                    }

                    if stored.has_components() {
                        continue;
                    }
                    tracing::info!("Replicating old tags for {name}...");
                    let components = stored.into_components();
                    for (name, tag_spec) in components.into_iter() {
                        let tag = self.inner.resolve_tag(&tag_spec).await?;
                        let new_tag_path = self.build_package_tag(&build).join(name.to_string());
                        let new_tag_spec = spfs::tracking::TagSpec::parse(&new_tag_path)?;

                        // NOTE(rbottriell): this copying process feels annoying
                        // and error prone. Ideally, there would be some set methods
                        // on the tag for changing the org/name on an existing one
                        let mut new_tag = spfs::tracking::Tag::new(
                            new_tag_spec.org(),
                            new_tag_spec.name(),
                            tag.target,
                        )?;
                        new_tag.parent = tag.parent;
                        new_tag.time = tag.time;
                        new_tag.user = tag.user;

                        self.insert_tag(&new_tag).await?;
                    }
                }
            }
        }
        meta.version = target_version;
        self.write_metadata(&meta).await?;
        // Note caches are already invalidated in `write_metadata`
        Ok("Repo up to date".to_string())
    }

    fn set_cache_policy(&self, cache_policy: CachePolicy) -> CachePolicy {
        let orig = self
            .cache_policy
            .swap(Box::leak(Box::new(cache_policy)), Ordering::Relaxed);

        // Safety: We only put valid `Box` pointers into `self.cache_policy`.
        *unsafe { Box::from_raw(orig) }
    }
}

impl SpfsRepository {
    fn cached_result_permitted(&self) -> bool {
        // Safety: We only put valid `Box` pointers into `self.cache_policy`.
        unsafe { *self.cache_policy.load(Ordering::Relaxed) }.cached_result_permitted()
    }

    async fn has_tag<F>(&self, for_pkg: F, tag: &tracking::TagSpec) -> bool
    where
        F: Fn() -> AnyIdent,
    {
        // This goes through the cache!
        self.resolve_tag(for_pkg, tag).await.is_ok()
    }

    /// Invalidate (clear) all cached results.
    fn invalidate_caches(&self) {
        self.caches.ls_tags.clear();
        self.caches.package_versions.clear();
        self.caches.recipe.clear();
        self.caches.package.clear();
        self.caches.tag_spec.clear();
        self.caches.list_build_components.clear();
    }

    async fn ls_tags(&self, path: &relative_path::RelativePath) -> Vec<Result<EntryType>> {
        if self.cached_result_permitted() {
            if let Some(v) = self.caches.ls_tags.get(path) {
                return v
                    .value()
                    .clone()
                    .into_iter()
                    .map(Ok)
                    .collect::<Vec<Result<EntryType>>>();
            }
        }
        let r: Vec<Result<EntryType>> = self
            .inner
            .ls_tags(path)
            .map(|el| el.map_err(|err| err.into()))
            .collect::<Vec<_>>()
            .await;

        self.caches.ls_tags.insert(
            path.to_owned(),
            r.iter().filter_map(|r| r.as_ref().ok()).cloned().collect(),
        );
        r
    }

    /// Read the metadata for this spk repository.
    ///
    /// The repo metadata contains information about
    /// how this particular spfs repository has been setup
    /// with spk. Namely, version and compatibility information.
    pub async fn read_metadata(&self) -> Result<RepositoryMetadata> {
        let tag_spec = spfs::tracking::TagSpec::parse(REPO_METADATA_TAG).unwrap();
        let digest = match self.inner.resolve_tag(&tag_spec).await {
            Ok(tag) => tag.target,
            Err(spfs::Error::UnknownReference(_)) => return Ok(Default::default()),
            Err(err) => return Err(err.into()),
        };
        let (mut reader, _) = self.inner.open_payload(digest).await?;
        let mut yaml = String::new();
        reader
            .read_to_string(&mut yaml)
            .await
            .map_err(|err| Error::FileReadError(digest.to_string().into(), err))?;
        let meta: RepositoryMetadata =
            serde_yaml::from_str(&yaml).map_err(Error::InvalidRepositoryMetadata)?;
        Ok(meta)
    }

    async fn resolve_tag<F>(
        &self,
        for_pkg: F,
        tag_spec: &tracking::TagSpec,
    ) -> Result<tracking::Tag>
    where
        F: Fn() -> AnyIdent,
    {
        if self.cached_result_permitted() {
            if let Some(v) = self.caches.tag_spec.get(tag_spec) {
                return v.value().clone().into();
            }
        }
        let r = self
            .inner
            .resolve_tag(tag_spec)
            .await
            .map_err(|err| match err {
                spfs::Error::UnknownReference(_) => Error::PackageNotFound(for_pkg()),
                err => err.into(),
            });

        self.caches
            .tag_spec
            .insert(tag_spec.clone(), r.as_ref().map(|el| el.clone()).into());
        r
    }

    /// Update the metadata for this spk repository.
    async fn write_metadata(&self, meta: &RepositoryMetadata) -> Result<()> {
        let tag_spec = spfs::tracking::TagSpec::parse(REPO_METADATA_TAG).unwrap();
        let yaml = serde_yaml::to_string(meta).map_err(Error::InvalidRepositoryMetadata)?;
        let digest = self
            .inner
            .commit_blob(Box::pin(std::io::Cursor::new(yaml.into_bytes())))
            .await?;
        self.inner.push_tag(&tag_spec, &digest).await?;
        self.invalidate_caches();
        Ok(())
    }

    /// Find a package stored in this repo in either the new or old way of tagging
    ///
    /// (with or without package components)
    async fn lookup_package(&self, pkg: &BuildIdent) -> Result<StoredPackage> {
        use spfs::tracking::TagSpec;
        let tag_path = self.build_package_tag(pkg);
        let tag_specs: HashMap<Component, TagSpec> = self
            .ls_tags(&tag_path)
            .await
            .into_iter()
            .filter_map(|entry| match entry {
                Ok(EntryType::Tag(name)) => Some(name),
                Ok(EntryType::Folder(_)) => None,
                Err(_) => None,
            })
            .filter_map(|e| Component::parse(&e).map(|c| (c, e)).ok())
            .filter_map(|(c, e)| TagSpec::parse(tag_path.join(e)).map(|p| (c, p)).ok())
            .collect();
        if !tag_specs.is_empty() {
            return Ok(StoredPackage::WithComponents(tag_specs));
        }
        let tag_spec = spfs::tracking::TagSpec::parse(&tag_path)?;
        if self.has_tag(|| pkg.to_any(), &tag_spec).await {
            return Ok(StoredPackage::WithoutComponents(tag_spec));
        }
        Err(Error::PackageNotFound(pkg.to_any()))
    }

    /// Construct an spfs tag string to represent a binary package layer.
    fn build_package_tag<T>(&self, pkg: &T) -> RelativePathBuf
    where
        T: TagPath,
    {
        let mut tag = RelativePathBuf::from("spk");
        tag.push("pkg");
        tag.push(pkg.tag_path());

        tag
    }

    /// Construct an spfs tag string to represent a spec file blob.
    fn build_spec_tag<T>(&self, pkg: &T) -> RelativePathBuf
    where
        T: TagPath,
    {
        let mut tag = RelativePathBuf::from("spk");
        tag.push("spec");
        tag.push(pkg.tag_path());

        tag
    }

    pub fn flush(&mut self) -> Result<()> {
        match &mut self.inner {
            spfs::storage::RepositoryHandle::Tar(tar) => Ok(tar.flush()?),
            _ => Ok(()),
        }
    }
}

#[derive(Deserialize, Serialize, Default, Debug, PartialEq, Eq)]
pub struct RepositoryMetadata {
    version: Version,
}

/// A simple enum that allows us to represent both the old and new form
/// of package storage as spfs tags.
enum StoredPackage {
    WithoutComponents(spfs::tracking::TagSpec),
    WithComponents(HashMap<Component, spfs::tracking::TagSpec>),
}

impl StoredPackage {
    /// true if this stored package uses the new format with
    /// tags for each package component
    fn has_components(&self) -> bool {
        matches!(self, Self::WithComponents(_))
    }

    /// Identify all of the tags associated with this package
    fn tags(&self) -> Vec<&spfs::tracking::TagSpec> {
        match &self {
            Self::WithoutComponents(tag) => vec![tag],
            Self::WithComponents(cmpts) => cmpts.values().collect(),
        }
    }

    /// Return the mapped component tags for this package, converting
    /// from the legacy storage format if needed.
    fn into_components(self) -> HashMap<Component, spfs::tracking::TagSpec> {
        match self {
            Self::WithComponents(cmpts) => cmpts,
            Self::WithoutComponents(tag) if tag.name() == "src" => {
                vec![(Component::Source, tag)].into_iter().collect()
            }
            Self::WithoutComponents(tag) => {
                vec![(Component::Build, tag.clone()), (Component::Run, tag)]
                    .into_iter()
                    .collect()
            }
        }
    }
}

/// Return the local packages repository used for development.
pub async fn local_repository() -> Result<SpfsRepository> {
    let config = spfs::get_config()?;
    let repo = config.get_local_repository().await?;
    let inner: spfs::prelude::RepositoryHandle = repo.into();
    let address = inner.address();
    Ok(SpfsRepository {
        caches: CachesForAddress::new(&address),
        address,
        name: "local".try_into()?,
        inner,
        cache_policy: AtomicPtr::new(Box::leak(Box::new(CachePolicy::CacheOk))),
    })
}

/// Return the remote repository of the given name.
///
/// If not name is specified, return the default spfs repository.
pub async fn remote_repository<S: AsRef<str>>(name: S) -> Result<SpfsRepository> {
    let config = spfs::get_config()?;
    let inner = config.get_remote(&name).await?;
    let address = inner.address();
    Ok(SpfsRepository {
        caches: CachesForAddress::new(&address),
        address,
        name: name.as_ref().try_into()?,
        inner,
        cache_policy: AtomicPtr::new(Box::leak(Box::new(CachePolicy::CacheOk))),
    })
}
