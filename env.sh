#!/bin/sh
# Source this file to set up the dsh-rs build environment.
export CARGO_HOME="$(cd "$(dirname "$0")" && pwd)/.cargo-home"
export PATH="$HOME/.cargo/bin:$PATH"
