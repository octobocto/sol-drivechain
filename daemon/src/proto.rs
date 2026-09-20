pub mod common {
    include!(concat!(env!("OUT_DIR"), "/cusf.common.v1.rs"));
}

pub mod mainchain {
    include!(concat!(env!("OUT_DIR"), "/cusf.mainchain.v1.rs"));
}
