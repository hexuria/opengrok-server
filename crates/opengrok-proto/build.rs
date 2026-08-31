fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Generated INTO target/, never into the tree: the .proto transcription is the artifact we
    // own and review; its codegen is derived, per docs/LEGAL.md's no-vendored-stubs rule.
    tonic_prost_build::compile_protos("proto/opengrok_seamb.proto")?;
    Ok(())
}
