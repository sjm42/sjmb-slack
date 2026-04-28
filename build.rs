// build.rs

// https://docs.rs/build-data/0.1.3/build_data/
fn main() -> anyhow::Result<()> {
    bd(build_data::set_GIT_BRANCH())?;
    bd(build_data::set_GIT_COMMIT())?;
    bd(build_data::set_SOURCE_TIMESTAMP())?;
    bd(build_data::set_RUSTC_VERSION())?;
    bd(build_data::no_debug_rebuilds())?;
    Ok(())
}

fn bd(result: Result<(), String>) -> anyhow::Result<()> {
    result.map_err(anyhow::Error::msg)
}
// EOF
