#!/usr/bin/env bash


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
    git clone "$repo" "$name" && cd "$name" ;
    cargo tarpaulin --debug &> "$name.log"
    mv "$name.log" "$results_dir"
    mv tarpaulin-run* "$results_dir/tarpaulin-run.json"
    cd .. 
    rm -rf "$name"
done
