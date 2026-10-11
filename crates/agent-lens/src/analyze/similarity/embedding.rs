//! `--method embedding`: cosine similarity of code-embedding vectors
//! computed by an ONNX model.
//!
//! The other methods compare what the parser saw; this one compares
//! what a model trained on code reads in the source text, so it can pair
//! two bodies that do the same thing through different syntax (the
//! Type-4 clone every structural score misses), at the cost of an
//! answer it cannot explain and a model file the binary does not ship.
//!
//! The model is a directory holding `model.onnx` (a sentence-embedding
//! encoder taking `input_ids` / `attention_mask` and optionally
//! `token_type_ids`) and the matching Hugging Face `tokenizer.json`.
//! Inference needs the `embedding` cargo feature; without it the method
//! fails with an error naming the feature rather than silently scoring
//! nothing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::corpus::OwnedUnit;
use crate::analyze::AnalyzerError;

/// Environment variable read when no model directory is configured.
pub const MODEL_ENV: &str = "AGENT_LENS_EMBEDDING_MODEL";

/// Resolve the model directory: the explicit option first, then
/// [`MODEL_ENV`].
pub(super) fn resolve_model_dir(explicit: Option<&Path>) -> Result<PathBuf, AnalyzerError> {
    explicit
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os(MODEL_ENV).map(PathBuf::from))
        .ok_or_else(|| {
            AnalyzerError::Embedding(format!(
                "--method embedding needs a model directory (model.onnx + tokenizer.json): pass --embedding-model or set {MODEL_ENV}"
            ))
        })
}

/// Unit-normalized embedding per corpus index in `wanted`. Units are
/// read back from disk by span, one read per file.
pub(super) fn embed_units(
    corpus: &[OwnedUnit],
    wanted: &[usize],
    model_dir: &Path,
) -> Result<HashMap<usize, Vec<f32>>, AnalyzerError> {
    let texts = unit_texts(corpus, wanted)?;
    let vectors = imp::embed_texts(
        model_dir,
        &texts.iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>(),
    )?;
    Ok(texts.into_iter().map(|(i, _)| i).zip(vectors).collect())
}

/// Cosine similarity of two unit-normalized vectors, clamped to
/// `[0.0, 1.0]` so it can feed the same blend and threshold as every
/// other body score. Anti-correlated code is not "more different" than
/// orthogonal code for the purpose of a duplicate report.
pub(super) fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| f64::from(*x) * f64::from(*y))
        .sum();
    dot.clamp(0.0, 1.0)
}

fn unit_texts(
    corpus: &[OwnedUnit],
    wanted: &[usize],
) -> Result<Vec<(usize, String)>, AnalyzerError> {
    let mut sources: HashMap<&Path, String> = HashMap::new();
    let mut out = Vec::with_capacity(wanted.len());
    for &i in wanted {
        let Some(unit) = corpus.get(i) else { continue };
        if !sources.contains_key(unit.file.as_path()) {
            let text = std::fs::read_to_string(&unit.file).map_err(|source| AnalyzerError::Io {
                path: unit.file.clone(),
                source,
            })?;
            sources.insert(unit.file.as_path(), text);
        }
        let Some(source) = sources.get(unit.file.as_path()) else {
            continue;
        };
        let text: Vec<&str> = source
            .lines()
            .skip(unit.start_line().saturating_sub(1))
            .take(unit.line_count())
            .collect();
        out.push((i, text.join("\n")));
    }
    Ok(out)
}

#[cfg(not(feature = "embedding"))]
mod imp {
    use std::path::Path;

    use crate::analyze::AnalyzerError;

    pub(super) fn embed_texts(
        _model_dir: &Path,
        _texts: &[&str],
    ) -> Result<Vec<Vec<f32>>, AnalyzerError> {
        Err(AnalyzerError::Embedding(
            "this agent-lens was built without the `embedding` feature; rebuild with `cargo install agent-lens --features embedding`".to_owned(),
        ))
    }
}

#[cfg(feature = "embedding")]
mod imp {
    use std::path::Path;
    use std::time::Instant;

    use ort::session::Session;
    use ort::value::Tensor;
    use tokenizers::Tokenizer;
    use tracing::debug;

    use super::super::PROFILE_TARGET;
    use crate::analyze::AnalyzerError;

    /// Tokens per model call. Encoders with longer context exist, but
    /// attention is quadratic in it and CPU inference is the target;
    /// a longer unit is split into windows of this size and their
    /// embeddings averaged by token count.
    const MAX_TOKENS: usize = 512;
    /// Windows per model call.
    const BATCH: usize = 16;

    fn err(context: &str, e: impl std::fmt::Display) -> AnalyzerError {
        AnalyzerError::Embedding(format!("{context}: {e}"))
    }

    /// One model-call window: which text it belongs to and its token ids,
    /// special tokens included.
    struct Window {
        text: usize,
        ids: Vec<i64>,
    }

    pub(super) fn embed_texts(
        model_dir: &Path,
        texts: &[&str],
    ) -> Result<Vec<Vec<f32>>, AnalyzerError> {
        let started = Instant::now();
        let mut tokenizer = Tokenizer::from_file(model_dir.join("tokenizer.json"))
            .map_err(|e| err("loading tokenizer.json", e))?;
        tokenizer
            .with_truncation(None)
            .map_err(|e| err("disabling truncation", e))?;
        tokenizer.with_padding(None);
        let threads = std::thread::available_parallelism().map_or(1, usize::from);
        let mut session = Session::builder()
            .map_err(|e| err("creating session", e))?
            .with_intra_threads(threads)
            .map_err(|e| err("configuring session", e))?
            .commit_from_file(model_dir.join("model.onnx"))
            .map_err(|e| err("loading model.onnx", e))?;
        let input_names: Vec<String> = session
            .inputs()
            .iter()
            .map(|i| i.name().to_owned())
            .collect();

        let encodings = tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| err("tokenizing", e))?;
        let mut windows: Vec<Window> = Vec::new();
        for (text, enc) in encodings.iter().enumerate() {
            windows.extend(
                split_windows(enc.get_ids())
                    .into_iter()
                    .map(|ids| Window { text, ids }),
            );
        }
        // Similar lengths batch together so padding stays small.
        windows.sort_by_key(|w| w.ids.len());

        let mut hidden = 0usize;
        let mut sums: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
        let mut weights: Vec<f32> = vec![0.0; texts.len()];
        for batch in windows.chunks(BATCH) {
            let pooled = run_batch(&mut session, &input_names, batch)?;
            for (window, vector) in batch.iter().zip(pooled) {
                hidden = vector.len();
                let weight = window.ids.len() as f32;
                let Some(sum) = sums.get_mut(window.text) else {
                    continue;
                };
                if sum.is_empty() {
                    sum.resize(hidden, 0.0);
                }
                for (s, v) in sum.iter_mut().zip(&vector) {
                    *s += v * weight;
                }
                if let Some(w) = weights.get_mut(window.text) {
                    *w += weight;
                }
            }
        }
        let out: Vec<Vec<f32>> = sums.into_iter().map(normalize).collect();
        debug!(
            target: PROFILE_TARGET,
            text_count = texts.len(),
            window_count = windows.len(),
            hidden,
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            "similarity embeddings computed"
        );
        Ok(out)
    }

    /// Split one encoding into windows of at most [`MAX_TOKENS`], each
    /// re-wrapped in the encoding's leading and trailing special token.
    fn split_windows(ids: &[u32]) -> Vec<Vec<i64>> {
        let ids: Vec<i64> = ids.iter().map(|&id| i64::from(id)).collect();
        if ids.len() <= MAX_TOKENS {
            return vec![ids];
        }
        let (Some(&first), Some(&last)) = (ids.first(), ids.last()) else {
            return Vec::new();
        };
        let body = ids.get(1..ids.len() - 1).unwrap_or_default();
        body.chunks(MAX_TOKENS - 2)
            .map(|chunk| {
                let mut window = Vec::with_capacity(chunk.len() + 2);
                window.push(first);
                window.extend_from_slice(chunk);
                window.push(last);
                window
            })
            .collect()
    }

    /// Run one padded batch and mean-pool each row over its attention
    /// mask. A model whose first output is already pooled (`[batch,
    /// hidden]`) is taken as is.
    fn run_batch(
        session: &mut Session,
        input_names: &[String],
        batch: &[Window],
    ) -> Result<Vec<Vec<f32>>, AnalyzerError> {
        let cols = batch.iter().map(|w| w.ids.len()).max().unwrap_or(0);
        let (ids, mask) = pad(batch, cols);
        let shape = [batch.len(), cols];
        let mut inputs: Vec<(String, ort::session::SessionInputValue<'_>)> = Vec::new();
        for name in input_names {
            let data = match name.as_str() {
                "input_ids" => ids.clone(),
                "attention_mask" => mask.clone(),
                "token_type_ids" => vec![0i64; ids.len()],
                other => {
                    return Err(AnalyzerError::Embedding(format!(
                        "model expects unsupported input {other:?}"
                    )));
                }
            };
            let tensor =
                Tensor::from_array((shape, data)).map_err(|e| err("building input tensor", e))?;
            inputs.push((name.clone(), tensor.into()));
        }
        let outputs = session.run(inputs).map_err(|e| err("running model", e))?;
        let (out_shape, data) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| err("reading model output", e))?;
        let dims: Vec<usize> = out_shape
            .iter()
            .map(|&d| usize::try_from(d).unwrap_or(0))
            .collect();
        match dims.as_slice() {
            [_, hidden] => Ok(data.chunks(*hidden).map(<[f32]>::to_vec).collect()),
            [_, _, hidden] => Ok(mean_pool(data, &mask, cols, *hidden)),
            other => Err(AnalyzerError::Embedding(format!(
                "unexpected model output shape {other:?}"
            ))),
        }
    }

    /// Right-pad every window to `cols` tokens: the flat id matrix and
    /// the attention mask marking which slots are real.
    fn pad(batch: &[Window], cols: usize) -> (Vec<i64>, Vec<i64>) {
        batch
            .iter()
            .flat_map(|w| (0..cols).map(|c| w.ids.get(c).map_or((0, 0), |&id| (id, 1))))
            .unzip()
    }

    /// Average each row's token vectors over the positions its mask
    /// marks real. `data` is `[rows, cols, hidden]` flattened.
    fn mean_pool(data: &[f32], mask: &[i64], cols: usize, hidden: usize) -> Vec<Vec<f32>> {
        data.chunks(cols * hidden)
            .zip(mask.chunks(cols))
            .map(|(row, row_mask)| {
                let mut pooled = vec![0.0f32; hidden];
                let mut real = 0usize;
                for (token, _) in row.chunks(hidden).zip(row_mask).filter(|&(_, &m)| m != 0) {
                    real += 1;
                    pooled.iter_mut().zip(token).for_each(|(p, v)| *p += v);
                }
                if real > 0 {
                    pooled.iter_mut().for_each(|p| *p /= real as f32);
                }
                pooled
            })
            .collect()
    }

    fn normalize(mut v: Vec<f32>) -> Vec<f32> {
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            v.iter_mut().for_each(|x| *x /= norm);
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::identical(&[0.6, 0.8], &[0.6, 0.8], 1.0)]
    #[case::orthogonal(&[1.0, 0.0], &[0.0, 1.0], 0.0)]
    #[case::opposite_clamps_to_zero(&[1.0, 0.0], &[-1.0, 0.0], 0.0)]
    fn cosine_of_unit_vectors(#[case] a: &[f32], #[case] b: &[f32], #[case] expected: f64) {
        assert!((cosine(a, b) - expected).abs() < 1e-6);
    }

    #[test]
    fn explicit_model_dir_wins() {
        let dir = resolve_model_dir(Some(Path::new("/models/code"))).unwrap();
        assert_eq!(dir, PathBuf::from("/models/code"));
    }
}
