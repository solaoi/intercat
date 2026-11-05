fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(&intercat_daemon::secret_paths::deny_globs()).unwrap()
    );
}
