//! The recommended `props.toml`, generated from the event structs.
//!
//! `indexed` in aglake's props.toml is what decides whether an aggregate reads
//! a column or decompresses a body. Measured on 124k real spans: with these
//! stanzas in place, every representative Heron query — the metrics rollups,
//! the Services page, the filter dropdowns — ran entirely off columns and
//! postings, with zero fallbacks to the row path. Without them the queries stay
//! *correct*, because aglake extracts fields at search time either way; they
//! just do it by decompressing bodies, which is one to two orders of magnitude
//! more work.
//!
//! # Why this is generated rather than written out
//!
//! A hand-kept field list is a second copy of the event schema, and a second
//! copy drifts: add a column to [`crate::rows::SpanEvent`], forget the props
//! file, and the deployment silently loses its fast path for that field, with
//! nothing failing anywhere. So the list comes from the structs themselves —
//! serde's derive already knows every field name, and [`field_names`] asks it.
//! New fields are indexed by default and blobs are excluded by rule, which
//! puts the failure on the side that is loud (a slightly larger columnar file)
//! rather than the side that is silent (a missing fast path).
//!
//! # Two limits an operator has to know
//!
//! `indexed` is read once, when aglaked starts, and **is never applied
//! retroactively**. Buckets written before the change keep whatever extraction
//! they had, so a query spanning the change point is fast on one side and slow
//! on the other, with nothing in the logs to say why. Plan a props change like
//! a schema migration even though aglake has no schema.
//!
//! And Heron never writes this file. It belongs to whoever operates aglaked,
//! sits in *their* data directory beside indexes this backend does not own,
//! and merging into it is their call — `heron aglake-props` prints, and stops
//! there.

use serde::de::{DeserializeOwned, Error as _};

use crate::rows::{
    FinishEvent, HttpEvent, MetricEvent, SpanEvent, TraceEvent, ST_BODY, ST_FINISH, ST_HTTP,
    ST_HTTP_BODY, ST_METRIC, ST_SPAN, ST_TRACE,
};

/// Fields excluded from extraction even though they are part of the event.
///
/// Two kinds. Anything ending in `_json` is a serialized array or object that
/// exists to be read back through serde, never to be filtered on — extracting
/// it would copy the blob into the columnar store for nothing, and
/// `span_ids_json` alone runs to tens of kilobytes per trace. The rest are free
/// text that is only ever projected: no query filters, groups, or sorts on a
/// preview string, so a column for it is pure cost.
///
/// Note what is *not* here: `models_used`, the native multivalue twin of
/// `models_used_json`. It exists precisely so the traces filter can match
/// against it, so the `_json` rule must not sweep it up with its twin — see
/// `models_used_stays_indexed`.
const NOT_INDEXED: &[&str] = &["user_input_preview", "final_answer_preview"];

fn is_blob(field: &str) -> bool {
    field.ends_with("_json") || NOT_INDEXED.contains(&field)
}

/// The complete field-name list behind a `Deserialize` impl.
///
/// serde's derive hands `deserialize_struct` the full static field list before
/// it looks at any data, which makes it the one place the names are available
/// without a second copy — and unlike serializing an instance, it sees fields
/// that `skip_serializing_if` would have hidden. Capturing them means
/// interrupting the deserializer, so the "error" here is the success path.
pub(crate) fn field_names<T: DeserializeOwned>() -> Vec<&'static str> {
    match T::deserialize(Sniffer) {
        Err(Captured(fields)) => fields,
        Ok(_) => Vec::new(),
    }
}

/// Carries the captured names out through the error channel.
#[derive(Debug)]
struct Captured(Vec<&'static str>);

impl std::fmt::Display for Captured {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "captured {} field names", self.0.len())
    }
}

impl std::error::Error for Captured {}

impl serde::de::Error for Captured {
    fn custom<T: std::fmt::Display>(_: T) -> Self {
        Captured(Vec::new())
    }
}

struct Sniffer;

impl<'de> serde::Deserializer<'de> for Sniffer {
    type Error = Captured;

    fn deserialize_struct<V: serde::de::Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        _visitor: V,
    ) -> std::result::Result<V::Value, Captured> {
        Err(Captured(fields.to_vec()))
    }

    fn deserialize_any<V: serde::de::Visitor<'de>>(
        self,
        _visitor: V,
    ) -> std::result::Result<V::Value, Captured> {
        // Only structs carry a field list; anything else has nothing to give.
        Err(Captured::custom("not a struct"))
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map enum identifier ignored_any
    }
}

/// The fields one sourcetype should extract at index time.
fn indexed_for<T: DeserializeOwned>() -> Vec<&'static str> {
    field_names::<T>()
        .into_iter()
        .filter(|f| !is_blob(f))
        .collect()
}

/// One props.toml stanza.
struct Stanza {
    sourcetype: &'static str,
    /// What this sourcetype is, for the comment above the stanza.
    about: &'static str,
    indexed: Vec<&'static str>,
    /// Bodies turn field extraction off entirely; see [`render`].
    auto_json: bool,
}

fn stanzas() -> Vec<Stanza> {
    vec![
        Stanza {
            sourcetype: ST_SPAN,
            about: "LLM call metadata — every filter, sort and aggregate in the calls, \
                    overview and services pages reads these",
            indexed: indexed_for::<SpanEvent>(),
            auto_json: true,
        },
        Stanza {
            sourcetype: ST_TRACE,
            about: "agent turns",
            indexed: indexed_for::<TraceEvent>(),
            auto_json: true,
        },
        Stanza {
            sourcetype: ST_METRIC,
            about: "pre-aggregated metric rows, one per (window, dimension tier)",
            indexed: indexed_for::<MetricEvent>(),
            auto_json: true,
        },
        Stanza {
            sourcetype: ST_FINISH,
            about: "finish-reason counts, kept apart from the wide metric rows because \
                    the columnar path needs one field set per bucket",
            indexed: indexed_for::<FinishEvent>(),
            auto_json: true,
        },
        Stanza {
            sourcetype: ST_HTTP,
            about: "raw HTTP exchange metadata",
            indexed: indexed_for::<HttpEvent>(),
            auto_json: true,
        },
        Stanza {
            sourcetype: ST_BODY,
            about: "request/response bodies and headers",
            indexed: Vec::new(),
            auto_json: false,
        },
        Stanza {
            sourcetype: ST_HTTP_BODY,
            about: "HTTP exchange bodies and headers",
            indexed: Vec::new(),
            auto_json: false,
        },
    ]
}

/// Render the props.toml stanzas Heron's indexes want.
pub fn render() -> String {
    let mut out = String::new();
    out.push_str(
        "# Heron — recommended aglake index-time extraction.\n\
         #\n\
         # Merge these stanzas into aglaked's <data-dir>/props.toml and restart\n\
         # aglaked. Everything here is a performance setting: queries return the\n\
         # same answers without it, but aggregates fall back to decompressing\n\
         # event bodies instead of reading columns and postings.\n\
         #\n\
         # Index-time settings apply to NEWLY INGESTED data only. Buckets already\n\
         # on disk keep the extraction they were written with, so a query that\n\
         # spans the change is fast on one side and slow on the other.\n\
         #\n\
         # Heron never writes this file — it is yours.\n",
    );
    for s in stanzas() {
        out.push_str(&format!("\n# {}\n[sourcetype.{}]\n", s.about, s.sourcetype));
        if s.auto_json {
            out.push_str("auto_json = true\n");
        } else {
            out.push_str(
                "# Field extraction off: these events are one large JSON blob whose\n\
                 # keys are not query targets, and flattening them would cost a parse\n\
                 # per event for nothing. Full-text search still works — raw bytes go\n\
                 # into the inverted index regardless of this setting, which is what\n\
                 # makes `search index=<prefix>_bodies \"SELECT * FROM users\"` find a\n\
                 # prompt by its contents.\n\
                 auto_json = false\n",
            );
        }
        if !s.indexed.is_empty() {
            out.push_str("indexed = [\n");
            for f in &s.indexed {
                out.push_str(&format!("  \"{f}\",\n"));
            }
            out.push_str("]\n");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_names_sees_fields_that_serialization_would_hide() {
        let names = field_names::<SpanEvent>();
        assert!(names.contains(&"id"));
        assert!(names.contains(&"wire_api"));
        // `Option` + `skip_serializing_if`: absent from a serialized default,
        // present here. This is the whole reason for the deserializer trick.
        assert!(
            names.contains(&"ttft_ms") && names.contains(&"process_pid"),
            "optional fields must be visible: {names:?}"
        );
        let json = serde_json::to_value(SpanEvent::default()).unwrap();
        assert!(
            json.get("ttft_ms").is_none(),
            "precondition: serializing a default really does hide it"
        );
    }

    /// The stanzas exist to make aggregates read columns. If a field the
    /// aggregates group or filter on is missing, the fast path is silently
    /// lost for it — so the fields those queries actually name are asserted
    /// here rather than left to a visual scan of the output.
    #[test]
    fn every_field_the_read_paths_filter_on_is_indexed() {
        let span = indexed_for::<SpanEvent>();
        for f in [
            "wire_api",
            "model",
            "server_ip",
            "server_port",
            "client_ip",
            "err",
            "err_class",
            "strm",
            "finish_reason",
            "ts_us",
            "source_id",
            "id",
            "request_path",
            "status_code",
            "app_hint",
            "server_header",
        ] {
            assert!(span.contains(&f), "spans must index {f}: {span:?}");
        }

        let metric = indexed_for::<MetricEvent>();
        for f in [
            "dim_tier",
            "ts_us",
            "wire_api",
            "model",
            "server_ip",
            "row_id",
        ] {
            assert!(metric.contains(&f), "metrics must index {f}");
        }

        let trace = indexed_for::<TraceEvent>();
        for f in [
            "turn_id",
            "session_id",
            "source_id",
            "agent_kind",
            "status",
            "proxy_hidden",
            "ts_us",
            "end_us",
            "first_span_id",
        ] {
            assert!(trace.contains(&f), "traces must index {f}");
        }
    }

    /// Serialized arrays and free-text previews are read back whole, never
    /// filtered — extracting them copies a blob into the columnar store for no
    /// query's benefit.
    #[test]
    fn blobs_and_previews_are_excluded() {
        let trace = indexed_for::<TraceEvent>();
        for f in [
            "span_ids_json",
            "metadata_json",
            "models_used_json",
            "user_input_preview",
            "final_answer_preview",
        ] {
            assert!(!trace.contains(&f), "traces must not index {f}");
            assert!(
                field_names::<TraceEvent>().contains(&f),
                "{f} must still be a real field — a rename would make this \
                 exclusion silently meaningless"
            );
        }
    }

    /// `models_used` is a multivalue field the traces filter matches against,
    /// which is exactly why it is written alongside its `_json` twin. The
    /// blob rule must not sweep it up with the twin.
    #[test]
    fn models_used_stays_indexed() {
        assert!(indexed_for::<TraceEvent>().contains(&"models_used"));
    }

    #[test]
    fn rendered_props_are_parseable_and_cover_every_sourcetype() {
        let text = render();
        for st in [
            ST_SPAN,
            ST_TRACE,
            ST_METRIC,
            ST_FINISH,
            ST_HTTP,
            ST_BODY,
            ST_HTTP_BODY,
        ] {
            assert!(
                text.contains(&format!("[sourcetype.{st}]")),
                "missing stanza for {st}"
            );
        }
        // Bodies must not get field extraction: a 320 KiB JSON body flattened
        // into hundreds of columns is the single most expensive mistake
        // available in this configuration.
        let bodies = text
            .split(&format!("[sourcetype.{ST_BODY}]"))
            .nth(1)
            .unwrap();
        let stanza = bodies.split("[sourcetype.").next().unwrap();
        assert!(stanza.contains("auto_json = false"));
        assert!(!stanza.contains("indexed = ["));

        let doc: toml_edit::DocumentMut = text.parse().expect("rendered props must be valid TOML");
        let span = &doc["sourcetype"][ST_SPAN];
        assert_eq!(span["auto_json"].as_bool(), Some(true));
        assert!(span["indexed"].as_array().unwrap().len() > 20);
        // A quoting bug would show up as a field name that no longer matches
        // the struct, so compare the parsed values against the source of truth.
        let rendered: Vec<String> = span["indexed"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(rendered, indexed_for::<SpanEvent>());
    }
}
