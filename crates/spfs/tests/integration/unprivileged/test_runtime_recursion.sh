#!/bin/bash

# Copyright (c) Sony Pictures Imageworks, et al.
# SPDX-License-Identifier: Apache-2.0
# https://github.com/spkenv/spk

set -o errexit

# test that spfs can be run from within spfs

out=$(spfs run '' -- sh -c 'spfs edit --off && spfs run - -- echo hello')
if [[ $out =~ 'hello' ]]; then exit 0; else exit 1; fi
