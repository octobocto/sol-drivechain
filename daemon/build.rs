use std::{env, fs, path::PathBuf};

use prost::Message;

const COMMON_PROTO: &str = "../bip300301_enforcer/proto/cusf/common/v1/common.proto";
const MAINCHAIN_COMMON_PROTO: &str = "../bip300301_enforcer/proto/cusf/mainchain/v1/common.proto";
const VALIDATOR_PROTO: &str = "../bip300301_enforcer/proto/cusf/mainchain/v1/validator.proto";
const BLOCK_PRODUCER_PROTO: &str =
    "../bip300301_enforcer/proto/cusf/mainchain/v1/block_producer.proto";
const MINING_PROTO: &str = "../bip300301_enforcer/proto/cusf/mainchain/v1/mining.proto";
const WALLET_PROTO: &str = "../bip300301_enforcer/proto/cusf/mainchain/v1/wallet.proto";
const INCLUDES: &[&str] = &["../bip300301_enforcer/proto"];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let all_protos = [
        COMMON_PROTO,
        MAINCHAIN_COMMON_PROTO,
        VALIDATOR_PROTO,
        BLOCK_PRODUCER_PROTO,
        MINING_PROTO,
        WALLET_PROTO,
    ];
    for proto in all_protos {
        println!("cargo:rerun-if-changed={proto}");
    }

    let file_descriptors = protox::compile(all_protos, INCLUDES)?;
    let file_descriptor_path = PathBuf::from(env::var("OUT_DIR")?).join("file_descriptor_set.bin");
    fs::write(&file_descriptor_path, file_descriptors.encode_to_vec())?;

    compile(&file_descriptor_path, &[COMMON_PROTO], |_| ())?;
    compile(
        &file_descriptor_path,
        &[
            MAINCHAIN_COMMON_PROTO,
            VALIDATOR_PROTO,
            BLOCK_PRODUCER_PROTO,
            MINING_PROTO,
            WALLET_PROTO,
        ],
        |config| {
            config.extern_path(".cusf.common.v1", "crate::proto::common");
        },
    )
}

fn compile(
    file_descriptor_path: &PathBuf,
    protos: &[&str],
    config_fn: impl FnOnce(&mut prost_build::Config),
) -> Result<(), Box<dyn std::error::Error>> {
    let mut config = prost_build::Config::new();
    config.enable_type_names();
    config_fn(&mut config);
    tonic_prost_build::configure()
        .skip_protoc_run()
        .file_descriptor_set_path(file_descriptor_path)
        .build_server(false)
        .compile_with_config(config, protos, INCLUDES)?;
    Ok(())
}
