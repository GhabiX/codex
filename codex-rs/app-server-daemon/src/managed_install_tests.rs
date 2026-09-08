use pretty_assertions::assert_eq;

use super::executable_identity_from_bytes;
use super::managed_codex_version;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn product_probe_does_not_confuse_upstream_compatibility_with_product_version() {
    let directory = tempfile::tempdir().expect("probe fixture");
    let executable = directory.path().join("codex");
    std::fs::write(&executable, r#"#!/bin/sh
case "$1" in
  --version) printf 'spine-codex 0.153.4\n' ;;
  mcp-server)
    read -r request
    case "$request" in
      *'"method":"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"codex-mcp-server","version":"0.4.0"}}}' ;;
      *) exit 2 ;;
    esac ;;
  *) exit 3 ;;
esac
"#).expect("write probe fixture");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
        .expect("make fixture executable");
    assert_eq!(
        managed_codex_version(&executable)
            .await
            .expect("product version"),
        "0.4.0"
    );
}

#[test]
fn executable_identity_uses_binary_contents() {
    let old = executable_identity_from_bytes(b"old");
    let same = executable_identity_from_bytes(b"old");
    let new = executable_identity_from_bytes(b"new");

    assert_eq!(old, same);
    assert_ne!(old, new);
}
