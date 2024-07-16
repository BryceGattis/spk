// Copyright (c) Sony Pictures Imageworks, et al.
// SPDX-License-Identifier: Apache-2.0
// https://github.com/spkenv/spk

use serde::{Deserialize, Serialize};

use crate::foundation::option_map::OptionMap;
use crate::ident::Request;
use crate::{Script, TestStage};

/// A set of structured inputs used to build a package.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct TestSpec {
    pub stage: TestStage,
    pub script: Script,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selectors: Vec<OptionMap>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requirements: Vec<Request>,
}

impl crate::Test for TestSpec {
    fn script(&self) -> String {
        self.script.join("\n")
    }

    fn additional_requirements(&self) -> Vec<Request> {
        self.requirements.clone()
    }
}
