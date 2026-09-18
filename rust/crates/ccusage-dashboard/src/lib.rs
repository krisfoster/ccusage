//! The dashboard bundle, carried inside the binary.
//!
//! There is no build step and no framework: the page is three hand-written
//! files. A bundler would buy nothing here — the bundle is a few tens of
//! kilobytes and has no dependencies — while costing a Node toolchain in every
//! build of a Rust binary, a lockfile in the supply chain, and non-determinism
//! in what ships. Embedding the assets means `ccusage sync dashboard` works
//! from a single downloaded binary with nothing else installed.
//!
//! Everything in here is world-readable once deployed, so nothing in these
//! files may be derived from the user: no bucket names, no machine ids, no
//! totals. The data arrives at runtime.

/// One file of the bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Asset {
    /// Path relative to the dashboard prefix.
    pub path: &'static str,
    pub content_type: &'static str,
    pub bytes: &'static [u8],
}

const INDEX: &str = include_str!("../assets/index.html");
const APP: &str = include_str!("../assets/app.js");
const STYLES: &str = include_str!("../assets/styles.css");

/// The bundle, in no particular order beyond the entry point first.
pub const ASSETS: &[Asset] = &[
    Asset {
        path: "index.html",
        content_type: "text/html; charset=utf-8",
        bytes: INDEX.as_bytes(),
    },
    Asset {
        path: "app.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: APP.as_bytes(),
    },
    Asset {
        path: "styles.css",
        content_type: "text/css; charset=utf-8",
        bytes: STYLES.as_bytes(),
    },
];

/// The budget the size spike settled on for the shell: a dashboard that is
/// slower to download than the CLI is to run has lost the plot. Uncompressed,
/// because that is what the binary carries and what the test can measure
/// without pulling in a compressor.
pub const BUNDLE_BUDGET_BYTES: usize = 128 * 1024;

pub fn total_bytes() -> usize {
    ASSETS.iter().map(|asset| asset.bytes.len()).sum()
}

pub fn asset(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|asset| asset.path == path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundle_stays_inside_its_size_budget() {
        assert!(
            total_bytes() <= BUNDLE_BUDGET_BYTES,
            "dashboard bundle is {} bytes, over the {BUNDLE_BUDGET_BYTES} byte budget",
            total_bytes()
        );
    }

    #[test]
    fn the_entry_point_is_served_first_and_by_name() {
        assert_eq!(ASSETS[0].path, "index.html");
        assert!(asset("styles.css").is_some());
        assert!(asset("../../etc/passwd").is_none());
    }

    /// There is no bundler and no Node in this build, so nothing else would
    /// notice the script asking for an element the page stopped containing:
    /// the panel would simply render as an empty box.
    #[test]
    fn every_element_the_script_fills_exists_in_the_page() {
        for id in ids_referenced_by(APP) {
            assert!(
                INDEX.contains(&format!("id=\"{id}\"")),
                "app.js fills #{id}, which index.html does not contain"
            );
        }
    }

    /// The chart is drawn, not laid out by CSS, so the classes it hands to the
    /// SVG are the only thing giving it axes, gridlines and bars.
    #[test]
    fn the_chart_classes_the_script_draws_are_styled() {
        for class in ["bar", "grid", "axis", "tick"] {
            assert!(
                APP.contains(&format!("class: '{class}'"))
                    || APP.contains(&format!("class: '{class} ")),
                "the chart no longer draws .{class}"
            );
            assert!(
                STYLES.contains(&format!(".series .{class}")),
                ".series .{class} is drawn but not styled"
            );
        }
    }

    fn ids_referenced_by(script: &str) -> Vec<&str> {
        script
            .match_indices("getElementById('")
            .filter_map(|(at, marker)| {
                let rest = &script[at + marker.len()..];
                rest.split_once('\'').map(|(id, _)| id)
            })
            .collect()
    }

    /// Everything here becomes world-readable, so a reference to the user's
    /// bucket, machine or spend would be a leak the moment it is deployed.
    #[test]
    fn no_asset_carries_anything_user_derived() {
        for asset in ASSETS {
            let text = std::str::from_utf8(asset.bytes).expect("assets are UTF-8");
            for forbidden in ["gs://", "storage.googleapis.com", "ccusage/v1/users"] {
                assert!(
                    !text.contains(forbidden),
                    "{} mentions {forbidden}",
                    asset.path
                );
            }
        }
    }
}
