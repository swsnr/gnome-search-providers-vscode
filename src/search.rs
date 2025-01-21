// Copyright Sebastian Wiesner <sebastian@swsnr.de>
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use std::{borrow::Cow, fmt::Debug};

use percent_encoding::percent_decode_str;
use tracing::{trace, warn};
use url::Url;

/// Calculate how well `uri` matches all of the given `terms`.
///
/// The URI gets scored for each term according to how far to the right it appears in the URI,
/// under the assumption that the right most part of an URI path is the most specific.
///
/// All matches are done on the lowercase text, i.e. case-insensitive.
///
/// Return a positive score if all of `terms` match `uri`.  The higher the score the
/// better the match, in relation to other matching values.  In and by itself however
/// the score has no intrinsic meaning.
///
/// If one term out of `terms` does not match `uri` return a score of 0, regardless
/// of how well other terms match.
#[allow(
    clippy::cast_precision_loss,
    reason = "terms won't grow so large as to cause issues in f64 conversion"
)]
fn score_uri<S: AsRef<str>>(uri: &str, terms: &[S]) -> f64 {
    let uri = uri.to_lowercase();
    terms
        .iter()
        .try_fold(0.0, |score, term| {
            uri.rfind(&term.as_ref().to_lowercase())
                // We add 1 to avoid returning zero if the term matches right at the beginning.
                .map(|index| score + ((index + 1) as f64 / uri.len() as f64))
        })
        .unwrap_or(0.0)
}

/// Find all URIs from `uris` which match all of `terms`.
///
/// Score every URI, and filter out all URIs with a score of 0 or less.
pub fn find_matching_uris<I, U, S>(uris: I, terms: &[S]) -> Vec<U>
where
    S: AsRef<str> + Debug,
    U: AsRef<str>,
    I: IntoIterator<Item = U>,
{
    let mut scored = uris
        .into_iter()
        .filter_map(|uri| {
            let decoded_uri = percent_decode_str(uri.as_ref()).decode_utf8().ok();
            let scored_uri = decoded_uri.as_deref().unwrap_or_else(|| uri.as_ref());
            let score = score_uri(scored_uri, terms);
            trace!("URI {scored_uri} scores {score} against {terms:?}");
            if score <= 0.0 {
                None
            } else {
                Some((score, uri))
            }
        })
        .collect::<Vec<_>>();
    #[allow(
        clippy::cast_possible_truncation,
        clippy::as_conversions,
        reason = "Truncation intended to calculate a coarse ordering score"
    )]
    scored.sort_by_key(|(score, _)| -((score * 1000.0) as i64));
    scored.into_iter().map(|(_, uri)| uri).collect::<Vec<_>>()
}

fn name_from_uri(uri_or_path: &str) -> Option<&str> {
    uri_or_path.split('/').filter(|seg| !seg.is_empty()).last()
}

/// Get the name and description for the given workspace URI or path.
pub fn name_and_description_of_uri(uri_or_path: &str) -> (String, String) {
    match Url::parse(uri_or_path) {
        Ok(parsed_uri) => {
            let decoded_path = percent_decode_str(parsed_uri.path()).decode_utf8().ok();
            let name = decoded_path
                .as_deref()
                .and_then(|path| name_from_uri(path).map(ToOwned::to_owned))
                .unwrap_or_else(|| uri_or_path.to_string());
            let description = match parsed_uri.scheme() {
                "file" if parsed_uri.host().is_none() => {
                    decoded_path.map_or_else(|| parsed_uri.path().to_string(), Cow::into_owned)
                }
                _ => percent_decode_str(uri_or_path)
                    .decode_utf8()
                    .ok()
                    .map_or_else(|| uri_or_path.to_string(), Cow::into_owned),
            };
            (name, description)
        }
        Err(error) => {
            warn!("Failed to parse {uri_or_path} as URI: {error}");
            let decoded = percent_decode_str(uri_or_path).decode_utf8().ok();
            let pretty_uri = decoded.as_deref().unwrap_or(uri_or_path);
            let name = name_from_uri(pretty_uri).unwrap_or(pretty_uri).to_string();
            let description = pretty_uri.to_string();
            (name, description)
        }
    }
}
