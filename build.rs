fn main() {
    embed_resource::compile("assets/vitals.rc", embed_resource::NONE)
        .manifest_required()
        .unwrap();
}
