extern crate prost_build;

fn main() {
    println!("cargo:rerun-if-changed=protos/traffic_stats.proto");
    prost_build::compile_protos(&["protos/traffic_stats.proto"], &["protos/"]).unwrap();
}
