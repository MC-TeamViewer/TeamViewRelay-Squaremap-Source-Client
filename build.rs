use std::env;
use std::path::{Path, PathBuf};

fn protocol_dir() -> PathBuf {
    if let Some(value) = env::var_os("TEAMVIEWRELAY_PROTOCOL_DIR") {
        return PathBuf::from(value);
    }

    for candidate in [
        Path::new("../TeamViewRelay-Protocol/proto"),
        Path::new("third_party/TeamViewRelay-Protocol/proto"),
    ] {
        if candidate.join("teamviewer/v1/teamviewer.proto").is_file() {
            return candidate.to_path_buf();
        }
    }

    panic!("TeamViewRelay protocol not found; set TEAMVIEWRELAY_PROTOCOL_DIR");
}

fn main() {
    let proto_dir = protocol_dir();
    let proto_file = proto_dir.join("teamviewer/v1/teamviewer.proto");
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
    unsafe {
        env::set_var("PROTOC", protoc);
    }
    println!("cargo:rerun-if-changed={}", proto_file.display());
    println!("cargo:rerun-if-env-changed=TEAMVIEWRELAY_PROTOCOL_DIR");
    prost_build::compile_protos(&[proto_file], &[proto_dir])
        .expect("compile TeamViewRelay protobuf");
}
