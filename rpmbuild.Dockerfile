FROM almalinux:9 as build_env

RUN yum install -y \
    epel-release \
    curl-minimal \
    rpm-build \
    yum-utils \
    && yum clean all

RUN ln -s cmake3 /usr/bin/cmake

# Rust Toolchain
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh /dev/stdin -y --default-toolchain=1.76.0
ENV PATH $PATH:/root/.cargo/bin

# Protobuf compiler (more recent than yum package)
ENV PB_REL="https://github.com/protocolbuffers/protobuf/releases"
RUN curl -LO $PB_REL/download/v3.15.8/protoc-3.15.8-linux-x86_64.zip
RUN unzip protoc-3.15.8-linux-x86_64.zip -d /

RUN mkdir -p /root/rpmbuild/{SOURCES,SPECS,RPMS,SRPMS}

FROM build_env as rpm_build

ARG VERSION
ARG APP

COPY ${APP}.spec /root/rpmbuild/SPECS/
ENV VERSION ${VERSION}
ENV APP ${APP}
RUN echo "Building $APP @ $VERSION"

# ensure the current build version matches the one in the rpm
# spec file, or things can go awry
RUN test "$VERSION" == "$(cat /root/rpmbuild/SPECS/$APP.spec | grep Version | cut -d ' ' -f 2)"

RUN yum-builddep -y /root/rpmbuild/SPECS/$APP.spec && yum clean all

COPY . /source/$APP-$VERSION
RUN tar -C /source -czvf /root/rpmbuild/SOURCES/v$VERSION.tar.gz .

ENTRYPOINT ["sh", "-c", "rpmbuild -ba /root/rpmbuild/SPECS/$APP.spec && chmod 0777 -R /root/rpmbuild/RPMS"]
