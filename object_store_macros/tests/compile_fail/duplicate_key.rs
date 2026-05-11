use object_store_macros::ObjectStoreConfig;

#[derive(ObjectStoreConfig)]
#[object_store(
    config_key = MyConfigKey,
    error_path = crate::Error::UnknownConfigurationKey
)]
struct MyOptions {
    #[config(key = "foo", strategy = option_string)]
    a: Option<String>,
    #[config(key = "foo", strategy = option_string)]
    b: Option<String>,
}

fn main() {}
