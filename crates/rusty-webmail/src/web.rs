//! The front end embedded in the binary (see `build.rs`) and the handler that serves it.

use std::sync::Arc;

use rusty_http::assets::{Asset, Assets};
use rusty_http::Handler;

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/assets.rs"));
}

/// Whether a front end was built into this binary.
pub fn is_embedded() -> bool {
    !embedded::FILES.is_empty()
}

/// The embedded files as assets (a file whose name or type the asset layer does not accept is left out, with a build-time-visible effect: the page would
/// miss it, which the bundle scan of the front end gates would have caught first).
pub fn assets() -> Vec<Asset> {
    embedded::FILES.iter().filter_map(|(path, bytes)| Asset::new(path, bytes)).collect()
}

/// The handler for `Router::fallback`: the static files and the single-page index; `None` when no front end is embedded.
pub fn handler() -> Option<Arc<dyn Handler>> {
    is_embedded().then(|| Assets::new(assets()).handler())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With a front end built in (the image build, or a developer who ran `vite build`), the index page is there and no file is a source map; without one the service is an API server and the table is empty.
    #[test]
    fn an_embedded_front_end_has_its_index_and_no_source_map() {
        if !is_embedded() {
            return;
        }
        assert!(embedded::FILES.iter().any(|(p, _)| *p == "/index.html"), "the index page is embedded");
        assert!(embedded::FILES.iter().all(|(p, _)| !p.ends_with(".map")));
    }
}
