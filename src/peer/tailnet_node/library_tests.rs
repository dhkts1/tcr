use super::DeviceFactory as _;

/// **Keys tcr mints are keys the library reads back**, through the key
/// file's opaque `key_state`, so a node that joined can be started by
/// the server from the file alone.
#[test]
fn minted_keys_round_trip_through_the_key_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = super::key_path(dir.path());
    let minted = super::library::LibraryFactory
        .mint_keys()
        .expect("fresh keys");
    super::write_key_file(
        &path,
        &super::NodeKeyFile {
            key_state: minted,
            hostname: Some("tcr-studio".to_string()),
        },
    )
    .expect("write");
    let read = super::read_key_file(&path).expect("read").expect("present");
    super::library::persist_state(&read).expect("the library reads its keys back");
}
