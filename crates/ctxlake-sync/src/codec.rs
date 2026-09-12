//! Envelope batch <-> Parquet, for one `sessions/.../seg-<n>.parquet` segment.
//!
//! Columns are the handful compaction and ad hoc queries reach for most
//! (`session_id`, `agent_id`, `event_type`, ...); the full envelope also rides
//! along verbatim as `envelope_json`. That is a deliberate simplification, not a
//! placeholder: decomposing every envelope field — including the nested
//! `tool`/`usage`/`injected_context` structures — into its own typed Parquet column
//! is real work with a real ongoing cost (`SCHEMA_VERSION`.rs says a bump only ever
//! *adds* a field, and each addition would need a matching column added here,
//! forever, in lockstep). Nothing downstream reads a per-field Parquet column yet —
//! DuckDB, Oxidant, and any other engine documented in AGENTS.md's "Relationship to
//! Oxidant" section read JSON columns natively — so shipping that decomposition now
//! would be a schema this crate has to keep backward-compatible for no reader.
//! Promote a field to its own column the day a real query needs to push a predicate
//! into it without parsing JSON first.

use std::sync::Arc;

use arrow::array::{Array, ArrayRef, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use bytes::Bytes;
use ctxlake_core::Envelope;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::arrow_writer::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;

/// Name of the column carrying the complete, original envelope. Public so a
/// consumer that only wants "the envelope, verbatim" (rather than the indexed
/// columns) has a documented, stable key instead of a magic string of its own.
pub const ENVELOPE_JSON_COLUMN: &str = "envelope_json";

fn schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("event_id", DataType::Utf8, false),
        Field::new("emitted_at", DataType::Utf8, false),
        Field::new("fleet_id", DataType::Utf8, false),
        Field::new("agent_id", DataType::Utf8, false),
        Field::new("runtime", DataType::Utf8, false),
        Field::new("session_id", DataType::Utf8, false),
        Field::new("event_type", DataType::Utf8, false),
        Field::new("content_hash", DataType::Utf8, false),
        Field::new("redaction_status", DataType::Utf8, false),
        Field::new(ENVELOPE_JSON_COLUMN, DataType::Utf8, false),
    ]))
}

/// `EventType`'s wire form (`"tool_call"`, not the Rust variant name) without
/// exposing a new public method on `ctxlake-core` just for this — `serde_json`
/// already computes exactly this string via the enum's own `#[serde(rename_all =
/// "snake_case")]`, so reusing it (through a string round trip) can never drift
/// from what the envelope itself serializes as.
fn event_type_str(e: &ctxlake_core::EventType) -> Result<String, String> {
    let quoted = serde_json::to_string(e).map_err(|err| err.to_string())?;
    Ok(quoted.trim_matches('"').to_string())
}

/// Encode one batch of envelopes as a single-row-group Parquet file, entirely in
/// memory. `envelopes` must be non-empty — an empty row group is a caller bug (the
/// upload loop only calls this once it has confirmed there is at least one new
/// line), not something to paper over with a zero-row file nobody asked for.
pub fn encode(envelopes: &[Envelope]) -> Result<Vec<u8>, String> {
    if envelopes.is_empty() {
        return Err("codec::encode: refusing to write an empty row group".to_string());
    }
    let schema = schema();

    let mut event_id = Vec::with_capacity(envelopes.len());
    let mut emitted_at = Vec::with_capacity(envelopes.len());
    let mut fleet_id = Vec::with_capacity(envelopes.len());
    let mut agent_id = Vec::with_capacity(envelopes.len());
    let mut runtime = Vec::with_capacity(envelopes.len());
    let mut session_id = Vec::with_capacity(envelopes.len());
    let mut event_type = Vec::with_capacity(envelopes.len());
    let mut content_hash = Vec::with_capacity(envelopes.len());
    let mut redaction_status = Vec::with_capacity(envelopes.len());
    let mut envelope_json = Vec::with_capacity(envelopes.len());

    for e in envelopes {
        event_id.push(e.event_id.clone());
        emitted_at.push(e.emitted_at.clone());
        fleet_id.push(e.fleet_id.clone());
        agent_id.push(e.agent_id.clone());
        runtime.push(e.runtime.as_str().to_string());
        session_id.push(e.session_id.clone());
        event_type.push(event_type_str(&e.event_type)?);
        content_hash.push(e.content_hash.clone());
        redaction_status.push(e.redaction.status.clone());
        envelope_json.push(e.to_ndjson().map_err(|err| err.to_string())?);
    }

    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            Arc::new(StringArray::from(event_id)) as ArrayRef,
            Arc::new(StringArray::from(emitted_at)) as ArrayRef,
            Arc::new(StringArray::from(fleet_id)) as ArrayRef,
            Arc::new(StringArray::from(agent_id)) as ArrayRef,
            Arc::new(StringArray::from(runtime)) as ArrayRef,
            Arc::new(StringArray::from(session_id)) as ArrayRef,
            Arc::new(StringArray::from(event_type)) as ArrayRef,
            Arc::new(StringArray::from(content_hash)) as ArrayRef,
            Arc::new(StringArray::from(redaction_status)) as ArrayRef,
            Arc::new(StringArray::from(envelope_json)) as ArrayRef,
        ],
    )
    .map_err(|e| e.to_string())?;

    let props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .build();
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut writer =
            ArrowWriter::try_new(&mut buf, schema, Some(props)).map_err(|e| e.to_string())?;
        writer.write(&batch).map_err(|e| e.to_string())?;
        writer.close().map_err(|e| e.to_string())?;
    }
    Ok(buf)
}

/// Decode a segment back into envelopes, re-parsed from the `envelope_json`
/// column. Used by this crate's own round-trip tests and available to anything
/// else (compaction, a future `ctxlake import` reader) that needs the original
/// envelopes back rather than the indexed columns.
pub fn decode(bytes: &[u8]) -> Result<Vec<Envelope>, String> {
    let bytes = Bytes::copy_from_slice(bytes);
    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes).map_err(|e| e.to_string())?;
    let reader = builder.build().map_err(|e| e.to_string())?;

    let mut out = Vec::new();
    for batch in reader {
        let batch = batch.map_err(|e| e.to_string())?;
        let col = batch
            .column_by_name(ENVELOPE_JSON_COLUMN)
            .ok_or_else(|| format!("segment is missing the {ENVELOPE_JSON_COLUMN} column"))?;
        let arr = col
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(|| format!("{ENVELOPE_JSON_COLUMN} column is not Utf8"))?;
        for i in 0..arr.len() {
            let envelope: Envelope =
                serde_json::from_str(arr.value(i)).map_err(|e| e.to_string())?;
            out.push(envelope);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctxlake_core::{EventType, Runtime};

    fn sample(session: &str, n: u32) -> Envelope {
        let mut e = Envelope::new(
            "oxidant",
            "cc-01",
            Runtime::ClaudeCode,
            session,
            EventType::ToolCall,
            format!("2026-09-11T18:22:{n:02}.000Z"),
        );
        e.content = Some(format!("event number {n}"));
        e.content_hash = ctxlake_core::hash::content_hash(e.content.as_deref().unwrap());
        e
    }

    fn as_json(e: &Envelope) -> serde_json::Value {
        serde_json::to_value(e).unwrap()
    }

    #[test]
    fn round_trips_every_field_through_the_envelope_json_column() {
        let envelopes = vec![sample("s1", 0), sample("s1", 1), sample("s1", 2)];
        let bytes = encode(&envelopes).unwrap();
        let back = decode(&bytes).unwrap();

        assert_eq!(back.len(), envelopes.len());
        for (original, roundtripped) in envelopes.iter().zip(back.iter()) {
            assert_eq!(
                as_json(original),
                as_json(roundtripped),
                "an envelope changed shape across a Parquet round trip"
            );
        }
    }

    #[test]
    fn refuses_to_encode_an_empty_batch() {
        let err = encode(&[]).unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
    }

    #[test]
    fn encoded_bytes_are_a_real_parquet_file() {
        // The four-byte "PAR1" magic footer/header every Parquet reader checks —
        // proof this is an actual Parquet file, not merely bytes `decode` happens
        // to accept.
        let bytes = encode(&[sample("s1", 0)]).unwrap();
        assert_eq!(&bytes[0..4], b"PAR1", "missing Parquet magic header");
        assert_eq!(
            &bytes[bytes.len() - 4..],
            b"PAR1",
            "missing Parquet magic footer"
        );
    }

    #[test]
    fn a_secret_that_never_reached_the_envelope_does_not_appear_via_encoding() {
        // Not a redaction test (redaction already ran in the hook, before this
        // module ever sees the envelope) — a regression guard that encoding itself
        // introduces no new path for raw bytes to leak around whatever content the
        // envelope actually carries.
        let mut e = sample("s1", 0);
        e.content = Some("[ctxlake: withheld, denied path .env]".to_string());
        let bytes = encode(&[e]).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains("AKIA"),
            "no secret material should appear regardless"
        );
    }
}
