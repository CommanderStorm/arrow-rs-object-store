use object_store_macros::ObjectStoreConfig;

#[derive(ObjectStoreConfig)]
#[object_store(
    config_key = MyConfigKey,
    error_path = crate::Error::UnknownConfigurationKey
)]
struct MyOptions {
    #[config(key = "", strategy = option_string)]
    foo: Option<String>,
}

fn main() {}
