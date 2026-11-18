fn main() {
    println!("cargo:rerun-if-env-changed=COLAB_CLI_BUILTIN_UPDATE_MANIFEST");
    let raw = std::env::var("COLAB_CLI_BUILTIN_UPDATE_MANIFEST").unwrap_or_default();
    let value = if raw.trim().is_empty() {
        String::new()
    } else {
        let uri = url::Url::parse(&raw).expect("内嵌发行源不是有效 URL");
        assert!(
            uri.scheme() == "https"
                && uri.has_host()
                && uri.username().is_empty()
                && uri.password().is_none()
                && uri.query().is_none()
                && uri.fragment().is_none(),
            "内嵌发行源必须为不含认证、query 或 fragment 的公开 HTTPS URL"
        );
        uri.to_string()
    };
    println!("cargo:rustc-env=COLAB_CLI_BUILTIN_UPDATE_MANIFEST={value}");
}
