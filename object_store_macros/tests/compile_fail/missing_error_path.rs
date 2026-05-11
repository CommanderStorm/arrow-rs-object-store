use object_store_macros::ObjectStoreConfig;

#[derive(ObjectStoreConfig)]
#[object_store(config_key = MyConfigKey)]
struct MyOptions {
    #[config(strategy = option_string)]
    foo: Option<String>,
}

fn main() {}
