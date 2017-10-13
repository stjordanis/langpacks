#!/bin/bash

set -e

echo "Installing Rust"
curl -sSf https://static.rust-lang.org/rustup.sh | sh -s -- --disable-sudo

echo "Installing sccache"
cargo install --vers 0.2.1 --root /usr/local --no-default-features sccache
rm -rf $HOME/.cargo
