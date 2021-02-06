#!/usr/bin/env bash

toolchain="+nightly"
echo "Running with toolchain $toolchain"
export RUST_LOG="cargo_tarpaulin=info"
mkdir results ;
mkdir projects ;
root=$PWD
cd projects
while read repo
do
    name="$(basename "$repo" .git)"
    results_dir="$root/results/$name"
    mkdir -p "$results_dir"
    echo "Cloning $name from $repo"
    echo "Saving results to $results_dir"
    git clone "$repo" "$name" &> /dev/null && cd "$name" ;
    echo "cargo $toolchain tarpaulin --debug --color never --all-features"
    cargo $toolchain tarpaulin --debug --color never --all-features &> "$name.log"
    mv "$name.log" "$results_dir"
    mv tarpaulin-run* "$results_dir/tarpaulin-run.json"
    cd .. 
    rm -rf "$name"
done
