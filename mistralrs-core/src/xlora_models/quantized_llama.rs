#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use std::collections::HashMap;
use std::iter::zip;

use candle_core::quantized::{ggml_file, gguf_file};
use candle_core::quantized::{QMatMul, QTensor};
use candle_core::{DType, Device, IndexOp, Result, Tensor, D};
use candle_nn::{Embedding, Module, VarBuilder};
use mistralrs_lora::{get_lora_cfg, LinearLayerLike, LoraConfig, Ordering, QLoraLinear};

use crate::models::Cache;

use super::classifier::XLoraClassifier;
use super::XLoraConfig;

pub const MAX_SEQ_LEN: u32 = 4096;

#[derive(Debug, Clone)]
struct RmsNorm {
    inner: candle_nn::LayerNorm,
    span: tracing::Span,
}

impl RmsNorm {
    fn new(scale: QTensor, eps: f32) -> Result<Self> {
        let span = tracing::span!(tracing::Level::TRACE, "rms-norm");
        let scale = scale.dequantize(&scale.device())?;
        let inner = candle_nn::LayerNorm::rms_norm(scale, eps as f64);
        Ok(Self { inner, span })
    }

    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let _enter = self.span.enter();
        self.inner.forward(x)
    }
}

#[derive(Debug)]
struct Mlp {
    feed_forward_w1: QLoraLinear,
    feed_forward_w2: QLoraLinear,
    feed_forward_w3: QLoraLinear,
}

impl Mlp {
    fn forward(&self, xs: &Tensor, scalings: Tensor, global_scaling_weight: f64) -> Result<Tensor> {
        let w1 = self
            .feed_forward_w1
            .lora_forward(xs, scalings.clone(), global_scaling_weight)?;
        let w3 = self
            .feed_forward_w3
            .lora_forward(xs, scalings.clone(), global_scaling_weight)?;
        self.feed_forward_w2.lora_forward(
            &(candle_nn::ops::silu(&w1)? * w3)?,
            scalings.clone(),
            global_scaling_weight,
        )
    }
}

#[derive(Debug)]
enum MlpOrMoe {
    Mlp(Mlp),
    MoE {
        n_expert_used: usize,
        feed_forward_gate_inp: QMatMul,
        experts: Vec<Mlp>,
    },
}

impl MlpOrMoe {
    fn forward(&self, xs: &Tensor, scalings: Tensor, global_scaling_weight: f64) -> Result<Tensor> {
        match self {
            Self::MoE {
                feed_forward_gate_inp,
                experts,
                n_expert_used,
            } => {
                let (b_size, seq_len, hidden_dim) = xs.dims3()?;
                let xs = xs.reshape(((), hidden_dim))?;
                let router_logits = feed_forward_gate_inp.forward(&xs)?;
                let routing_weights = candle_nn::ops::softmax_last_dim(&router_logits)?;

                // In order to extract topk, we extract the data from the tensor and manipulate it
                // directly. Maybe we will want to use some custom ops instead at some point.
                let routing_weights = routing_weights.to_dtype(DType::F32)?.to_vec2::<f32>()?;

                // routing_weights, selected_experts = torch.topk(routing_weights, self.top_k, dim=-1)
                // top_x contains the row indexes to evaluate for each expert.
                let mut top_x = vec![vec![]; experts.len()];
                let mut selected_rws = vec![vec![]; experts.len()];
                for (row_idx, rw) in routing_weights.iter().enumerate() {
                    let mut dst = (0..rw.len() as u32).collect::<Vec<u32>>();
                    dst.sort_by(|&i, &j| rw[j as usize].total_cmp(&rw[i as usize]));
                    let mut sum_routing_weights = 0f32;
                    for &expert_idx in dst.iter().take(*n_expert_used) {
                        let expert_idx = expert_idx as usize;
                        let routing_weight = rw[expert_idx];
                        sum_routing_weights += routing_weight;
                        top_x[expert_idx].push(row_idx as u32);
                    }
                    for &expert_idx in dst.iter().take(*n_expert_used) {
                        let expert_idx = expert_idx as usize;
                        let routing_weight = rw[expert_idx];
                        selected_rws[expert_idx].push(routing_weight / sum_routing_weights)
                    }
                }

                // routing_weights /= routing_weights.sum(dim=-1, keepdim=True)
                // expert_mask = torch.nn.functional.one_hot(selected_experts, num_classes=self.num_experts).permute(2, 1, 0)

                let mut ys = xs.zeros_like()?;
                for (expert_idx, expert_layer) in experts.iter().enumerate() {
                    let top_x = &top_x[expert_idx];
                    if top_x.is_empty() {
                        continue;
                    }
                    let top_x = Tensor::new(top_x.as_slice(), xs.device())?;
                    let selected_rws =
                        Tensor::new(selected_rws[expert_idx].as_slice(), xs.device())?
                            .reshape(((), 1))?;
                    // Index the correct hidden states and compute the expert hidden state for
                    // the current expert. We need to make sure to multiply the output hidden
                    // states by `routing_weights` on the corresponding tokens (top-1 and top-2)
                    let current_state = xs.index_select(&top_x, 0)?.reshape(((), hidden_dim))?;
                    // current_hidden_states = expert_layer(current_state, routing_weights[top_x_list, idx_list, None])
                    let current_hidden_states = expert_layer.forward(
                        &current_state,
                        scalings.clone(),
                        global_scaling_weight,
                    )?;
                    let current_hidden_states =
                        current_hidden_states.broadcast_mul(&selected_rws)?;
                    ys = ys.index_add(&top_x, &current_hidden_states, 0)?;
                }

                let ys = ys.reshape((b_size, seq_len, hidden_dim))?;
                Ok(ys)
            }
            Self::Mlp(mlp) => mlp.forward(xs, scalings.clone(), global_scaling_weight),
        }
    }
}

#[derive(Debug)]
struct LayerWeights {
    attention_wq: QLoraLinear,
    attention_wk: QLoraLinear,
    attention_wv: QLoraLinear,
    attention_wo: QLoraLinear,
    attention_norm: RmsNorm,
    mlp_or_moe: MlpOrMoe,
    ffn_norm: RmsNorm,
    n_head: usize,
    n_kv_head: usize,
    head_dim: usize,
    cos: Tensor,
    sin: Tensor,
    span_attn: tracing::Span,
    span_rot: tracing::Span,
    span_mlp: tracing::Span,
}

fn masked_fill(on_false: &Tensor, mask: &Tensor, on_true: f32) -> Result<Tensor> {
    let shape = mask.shape();
    let on_true = Tensor::new(on_true, on_false.device())?.broadcast_as(shape.dims())?;
    let m = mask.where_cond(&on_true, on_false)?;
    Ok(m)
}

impl LayerWeights {
    fn apply_rotary_emb(&self, x: &Tensor, seqlen_offsets: &[usize]) -> Result<Tensor> {
        let _enter = self.span_rot.enter();
        let (b_sz, n_head, seq_len, n_embd) = x.dims4()?;
        let mut ropes = Vec::new();
        let x = x.reshape((b_sz, n_head, seq_len, n_embd / 2, 2))?;
        for (b, seqlen_offset) in zip(0..b_sz, seqlen_offsets) {
            let cos =
                self.cos
                    .narrow(0, *seqlen_offset, seq_len)?
                    .reshape((seq_len, n_embd / 2, 1))?;
            let sin =
                self.sin
                    .narrow(0, *seqlen_offset, seq_len)?
                    .reshape((seq_len, n_embd / 2, 1))?;
            let cos = cos.broadcast_as((1, 1, seq_len, n_embd / 2, 1))?;
            let sin = sin.broadcast_as((1, 1, seq_len, n_embd / 2, 1))?;
            // This mimics the llama.cpp behavior.
            // https://github.com/ggerganov/llama.cpp/blob/1f0bccb27929e261744c979bc75114955da49e98/ggml.c#L12104-L12105
            // The x0 and x1 value are interleaved on the n_embd (= head_dim) dimension.
            // The resulting y0 and y1 are also interleaved with:
            //   y0 = x0*cos - x1*sin
            //   y1 = x0*sin + x1*cos
            let x_b = x.i(b)?.unsqueeze(0)?;
            let x0 = x_b.narrow(D::Minus1, 0, 1)?;
            let x1 = x_b.narrow(D::Minus1, 1, 1)?;
            let y0 = (x0.broadcast_mul(&cos)? - x1.broadcast_mul(&sin)?)?;
            let y1 = (x0.broadcast_mul(&sin)? + x1.broadcast_mul(&cos)?)?;
            let rope = Tensor::cat(&[y0, y1], D::Minus1)?;
            let rope = rope.flatten_from(D::Minus2)?;
            ropes.push(rope);
        }
        Tensor::cat(&ropes, 0)
    }

    fn forward_attn(
        &mut self,
        x: &Tensor,
        mask: &Tensor,
        start_offsets: &[usize],
        kv_cache: &mut Option<(Tensor, Tensor)>,
        scalings: Tensor,
        global_scaling_weight: f64,
    ) -> Result<Tensor> {
        let _enter = self.span_attn.enter();
        let (b_sz, seq_len, n_embd) = x.dims3()?;
        let q = self
            .attention_wq
            .lora_forward(x, scalings.clone(), global_scaling_weight)?;
        let k = self
            .attention_wk
            .lora_forward(x, scalings.clone(), global_scaling_weight)?;
        let v = self
            .attention_wv
            .lora_forward(x, scalings.clone(), global_scaling_weight)?;

        let q = q
            .reshape((b_sz, seq_len, self.n_head, self.head_dim))?
            .transpose(1, 2)?;
        let k = k
            .reshape((b_sz, seq_len, self.n_kv_head, self.head_dim))?
            .transpose(1, 2)?;
        let v = v
            .reshape((b_sz, seq_len, self.n_kv_head, self.head_dim))?
            .transpose(1, 2)?;

        let q = self.apply_rotary_emb(&q, start_offsets)?;
        let k = self.apply_rotary_emb(&k, start_offsets)?;

        let (k, v) = match &*kv_cache {
            None => (k, v),
            Some((k_cache, v_cache)) => {
                let k = Tensor::cat(&[k_cache, &k], 2)?.contiguous()?;
                let v = Tensor::cat(&[v_cache, &v], 2)?.contiguous()?;
                (k, v)
            }
        };
        *kv_cache = Some((k.clone(), v.clone()));

        // Support for MQA, useful for 70B models.
        let k = self.repeat_kv(k)?;
        let v = self.repeat_kv(v)?;

        let att = (q.matmul(&k.t()?)? / (self.head_dim as f64).sqrt())?;
        let mask = mask.broadcast_as(att.shape())?;
        let att = masked_fill(&att, &mask, f32::NEG_INFINITY)?;
        let att = candle_nn::ops::softmax_last_dim(&att)?;
        // Convert to contiguous as matmul doesn't support strided vs for now.
        let y = att.matmul(&v.contiguous()?)?;
        let y = y.transpose(1, 2)?.reshape(&[b_sz, seq_len, n_embd])?;
        let y = self
            .attention_wo
            .lora_forward(&y, scalings.clone(), global_scaling_weight)?;
        Ok(y)
    }

    fn repeat_kv(&self, x: Tensor) -> Result<Tensor> {
        let n_rep = self.n_head / self.n_kv_head;
        if n_rep == 1 {
            Ok(x)
        } else {
            let (b_sz, n_kv_head, seq_len, head_dim) = x.dims4()?;
            let x = x
                .unsqueeze(2)?
                .expand((b_sz, n_kv_head, n_rep, seq_len, head_dim))?
                .reshape((b_sz, n_kv_head * n_rep, seq_len, head_dim))?;
            Ok(x)
        }
    }
}

pub struct ModelWeights {
    tok_embeddings: Embedding,
    layers: Vec<LayerWeights>,
    norm: RmsNorm,
    output: QMatMul,
    masks: HashMap<usize, Tensor>,
    span: tracing::Span,
    pub device: Device,
    pub cache: Cache,
    xlora_classifier: XLoraClassifier,
}

fn precomput_freqs_cis(
    head_dim: usize,
    freq_base: f32,
    device: &Device,
) -> Result<(Tensor, Tensor)> {
    let theta: Vec<_> = (0..head_dim)
        .step_by(2)
        .map(|i| 1f32 / freq_base.powf(i as f32 / head_dim as f32))
        .collect();
    let theta = Tensor::new(theta.as_slice(), device)?;
    let idx_theta = Tensor::arange(0, MAX_SEQ_LEN, device)?
        .to_dtype(DType::F32)?
        .reshape((MAX_SEQ_LEN as usize, 1))?
        .matmul(&theta.reshape((1, theta.elem_count()))?)?;
    let cos = idx_theta.cos()?;
    let sin = idx_theta.sin()?;
    Ok((cos, sin))
}

impl ModelWeights {
    pub fn from_ggml(
        mut ct: ggml_file::Content,
        gqa: usize,
        lora_config: &[(String, LoraConfig)],
        vb: &VarBuilder,
        ordering: &Ordering,
        xlora_config: XLoraConfig,
    ) -> Result<Self> {
        let head_dim = (ct.hparams.n_embd / ct.hparams.n_head) as usize;
        let (cos, sin) = precomput_freqs_cis(head_dim, 10000., &ct.device)?;
        let tok_embeddings = ct.remove("tok_embeddings.weight")?;
        let tok_embeddings = tok_embeddings.dequantize(&ct.device)?;
        let norm = RmsNorm::new(ct.remove("norm.weight")?, 1e-5)?;
        let output = ct.remove("output.weight")?;
        let mut layers = Vec::with_capacity(ct.hparams.n_layer as usize);
        let mut count = 0;
        for layer_idx in 0..ct.hparams.n_layer {
            let prefix = format!("layers.{layer_idx}");
            let attention_wq = ct.remove(&format!("{prefix}.attention.wq.weight"))?;
            let attention_wk = ct.remove(&format!("{prefix}.attention.wk.weight"))?;
            let attention_wv = ct.remove(&format!("{prefix}.attention.wv.weight"))?;
            let attention_wo = ct.remove(&format!("{prefix}.attention.wo.weight"))?;
            let mlp_or_moe = {
                let feed_forward_w1 = ct.remove(&format!("{prefix}.feed_forward.w1.weight"))?;
                let feed_forward_w2 = ct.remove(&format!("{prefix}.feed_forward.w2.weight"))?;
                let feed_forward_w3 = ct.remove(&format!("{prefix}.feed_forward.w3.weight"))?;
                let cfg_w1 = get_lora_cfg(&feed_forward_w1);
                let cfg_w2 = get_lora_cfg(&feed_forward_w2);
                let cfg_w3 = get_lora_cfg(&feed_forward_w3);
                MlpOrMoe::Mlp(Mlp {
                    feed_forward_w1: QLoraLinear::new(
                        QMatMul::from_qtensor(feed_forward_w1)?,
                        &cfg_w1,
                        lora_config,
                        vb,
                        ordering,
                        format!("model.layers.{layer_idx}.mlp.gate_proj"),
                        &mut count,
                    )?,
                    feed_forward_w2: QLoraLinear::new(
                        QMatMul::from_qtensor(feed_forward_w2)?,
                        &cfg_w2,
                        lora_config,
                        vb,
                        ordering,
                        format!("model.layers.{layer_idx}.mlp.down_proj"),
                        &mut count,
                    )?,
                    feed_forward_w3: QLoraLinear::new(
                        QMatMul::from_qtensor(feed_forward_w3)?,
                        &cfg_w3,
                        lora_config,
                        vb,
                        ordering,
                        format!("model.layers.{layer_idx}.mlp.up_proj"),
                        &mut count,
                    )?,
                })
            };
            let attention_norm = ct.remove(&format!("{prefix}.attention_norm.weight"))?;
            let ffn_norm = ct.remove(&format!("{prefix}.ffn_norm.weight"))?;
            let span_attn = tracing::span!(tracing::Level::TRACE, "attn");
            let span_rot = tracing::span!(tracing::Level::TRACE, "attn-rot");
            let span_mlp = tracing::span!(tracing::Level::TRACE, "attn-mlp");
            let cfgq = get_lora_cfg(&attention_wq);
            let cfgk = get_lora_cfg(&attention_wk);
            let cfgv = get_lora_cfg(&attention_wv);
            let cfgo = get_lora_cfg(&attention_wo);
            layers.push(LayerWeights {
                attention_wq: QLoraLinear::new(
                    QMatMul::from_qtensor(attention_wq)?,
                    &cfgq,
                    lora_config,
                    vb,
                    ordering,
                    format!("model.layers.{layer_idx}.self_attn.q_proj"),
                    &mut count,
                )?,
                attention_wk: QLoraLinear::new(
                    QMatMul::from_qtensor(attention_wk)?,
                    &cfgk,
                    lora_config,
                    vb,
                    ordering,
                    format!("model.layers.{layer_idx}.self_attn.k_proj"),
                    &mut count,
                )?,
                attention_wv: QLoraLinear::new(
                    QMatMul::from_qtensor(attention_wv)?,
                    &cfgv,
                    lora_config,
                    vb,
                    ordering,
                    format!("model.layers.{layer_idx}.self_attn.v_proj"),
                    &mut count,
                )?,
                attention_wo: QLoraLinear::new(
                    QMatMul::from_qtensor(attention_wo)?,
                    &cfgo,
                    lora_config,
                    vb,
                    ordering,
                    format!("model.layers.{layer_idx}.self_attn.o_proj"),
                    &mut count,
                )?,
                attention_norm: RmsNorm::new(attention_norm, 1e-5)?,
                mlp_or_moe,
                ffn_norm: RmsNorm::new(ffn_norm, 1e-5)?,
                n_head: ct.hparams.n_head as usize,
                n_kv_head: ct.hparams.n_head as usize / gqa,
                head_dim: (ct.hparams.n_embd / ct.hparams.n_head) as usize,
                cos: cos.clone(),
                sin: sin.clone(),
                span_attn,
                span_rot,
                span_mlp,
            })
        }
        let span = tracing::span!(tracing::Level::TRACE, "model");
        Ok(Self {
            tok_embeddings: Embedding::new(tok_embeddings, ct.hparams.n_embd as usize),
            layers,
            norm,
            output: QMatMul::from_qtensor(output)?,
            masks: HashMap::new(),
            span,
            device: ct.device.clone(),
            cache: Cache::new(ct.hparams.n_layer as usize, true),
            xlora_classifier: XLoraClassifier::new(
                xlora_config,
                count,
                lora_config.len(),
                vb.clone(),
                true,
            )?,
        })
    }

    pub fn from_gguf<R: std::io::Seek + std::io::Read>(
        ct: gguf_file::Content,
        reader: &mut R,
        device: &Device,
        lora_config: &[(String, LoraConfig)],
        vb: &VarBuilder,
        ordering: &Ordering,
        xlora_config: XLoraConfig,
    ) -> Result<Self> {
        let md_get = |s: &str| match ct.metadata.get(s) {
            None => candle_core::bail!("cannot find {s} in metadata"),
            Some(v) => Ok(v),
        };

        // Parameter extraction from metadata.
        let n_expert = md_get("llama.expert_count")
            .and_then(|v| v.to_u32())
            .unwrap_or(0) as usize;
        let n_expert_used = md_get("llama.expert_used_count")
            .and_then(|v| v.to_u32())
            .unwrap_or(0) as usize;
        let head_count = md_get("llama.attention.head_count")?.to_u32()? as usize;
        let head_count_kv = md_get("llama.attention.head_count_kv")?.to_u32()? as usize;
        let block_count = md_get("llama.block_count")?.to_u32()? as usize;
        let embedding_length = md_get("llama.embedding_length")?.to_u32()? as usize;
        let rope_dim = md_get("llama.rope.dimension_count")?.to_u32()? as usize;
        // Strangely this value is generally 1e-6 in GGUF file but used to be 1e-5 by default.
        let rms_norm_eps = md_get("llama.attention.layer_norm_rms_epsilon")?.to_f32()?;

        let rope_freq_base = md_get("llama.rope.freq_base")
            .and_then(|m| m.to_f32())
            .unwrap_or(10000f32);
        let (cos, sin) = precomput_freqs_cis(rope_dim, rope_freq_base, device)?;

        let tok_embeddings = ct.tensor(reader, "token_embd.weight", device)?;
        let tok_embeddings = tok_embeddings.dequantize(device)?;
        let norm = RmsNorm::new(
            ct.tensor(reader, "output_norm.weight", device)?,
            rms_norm_eps,
        )?;
        let output = ct.tensor(reader, "output.weight", device)?;
        let mut layers = Vec::with_capacity(block_count);
        let mut count = 0;
        for layer_idx in 0..block_count {
            let prefix = format!("blk.{layer_idx}");
            let attention_wq = ct.tensor(reader, &format!("{prefix}.attn_q.weight"), device)?;
            let attention_wk = ct.tensor(reader, &format!("{prefix}.attn_k.weight"), device)?;
            let attention_wv = ct.tensor(reader, &format!("{prefix}.attn_v.weight"), device)?;
            let attention_wo =
                ct.tensor(reader, &format!("{prefix}.attn_output.weight"), device)?;
            let mlp_or_moe = if n_expert <= 1 {
                let feed_forward_w1 =
                    ct.tensor(reader, &format!("{prefix}.ffn_gate.weight"), device)?;
                let feed_forward_w2 =
                    ct.tensor(reader, &format!("{prefix}.ffn_down.weight"), device)?;
                let feed_forward_w3 =
                    ct.tensor(reader, &format!("{prefix}.ffn_up.weight"), device)?;
                let cfg_w1 = get_lora_cfg(&feed_forward_w1);
                let cfg_w2 = get_lora_cfg(&feed_forward_w2);
                let cfg_w3 = get_lora_cfg(&feed_forward_w3);
                MlpOrMoe::Mlp(Mlp {
                    feed_forward_w1: QLoraLinear::new(
                        QMatMul::from_qtensor(feed_forward_w1)?,
                        &cfg_w1,
                        lora_config,
                        vb,
                        ordering,
                        format!("model.layers.{layer_idx}.mlp.gate_proj"),
                        &mut count,
                    )?,
                    feed_forward_w2: QLoraLinear::new(
                        QMatMul::from_qtensor(feed_forward_w2)?,
                        &cfg_w2,
                        lora_config,
                        vb,
                        ordering,
                        format!("model.layers.{layer_idx}.mlp.down_proj"),
                        &mut count,
                    )?,
                    feed_forward_w3: QLoraLinear::new(
                        QMatMul::from_qtensor(feed_forward_w3)?,
                        &cfg_w3,
                        lora_config,
                        vb,
                        ordering,
                        format!("model.layers.{layer_idx}.mlp.up_proj"),
                        &mut count,
                    )?,
                })
            } else {
                let feed_forward_gate_inp =
                    ct.tensor(reader, &format!("{prefix}.ffn_gate_inp.weight"), device)?;
                let mut experts = Vec::with_capacity(n_expert);
                for i in 0..n_expert {
                    let feed_forward_w1 =
                        ct.tensor(reader, &format!("{prefix}.ffn_gate.{i}.weight"), device)?;
                    let feed_forward_w2 =
                        ct.tensor(reader, &format!("{prefix}.ffn_down.{i}.weight"), device)?;
                    let feed_forward_w3 =
                        ct.tensor(reader, &format!("{prefix}.ffn_up.{i}.weight"), device)?;
                    let cfg_w1 = get_lora_cfg(&feed_forward_w1);
                    let cfg_w2 = get_lora_cfg(&feed_forward_w2);
                    let cfg_w3 = get_lora_cfg(&feed_forward_w3);
                    experts.push(Mlp {
                        feed_forward_w1: QLoraLinear::new(
                            QMatMul::from_qtensor(feed_forward_w1)?,
                            &cfg_w1,
                            lora_config,
                            vb,
                            ordering,
                            format!("model.layers.{layer_idx}.mlp.gate_proj.{i}"),
                            &mut count,
                        )?,
                        feed_forward_w2: QLoraLinear::new(
                            QMatMul::from_qtensor(feed_forward_w2)?,
                            &cfg_w2,
                            lora_config,
                            vb,
                            ordering,
                            format!("model.layers.{layer_idx}.mlp.down_proj.{i}"),
                            &mut count,
                        )?,
                        feed_forward_w3: QLoraLinear::new(
                            QMatMul::from_qtensor(feed_forward_w3)?,
                            &cfg_w3,
                            lora_config,
                            vb,
                            ordering,
                            format!("model.layers.{layer_idx}.mlp.up_proj.{i}"),
                            &mut count,
                        )?,
                    })
                }
                MlpOrMoe::MoE {
                    n_expert_used,
                    feed_forward_gate_inp: QMatMul::from_qtensor(feed_forward_gate_inp)?,
                    experts,
                }
            };
            let attention_norm =
                ct.tensor(reader, &format!("{prefix}.attn_norm.weight"), device)?;
            let ffn_norm = ct.tensor(reader, &format!("{prefix}.ffn_norm.weight"), device)?;
            let span_attn = tracing::span!(tracing::Level::TRACE, "attn");
            let span_rot = tracing::span!(tracing::Level::TRACE, "attn-rot");
            let span_mlp = tracing::span!(tracing::Level::TRACE, "attn-mlp");
            let cfgq = get_lora_cfg(&attention_wq);
            let cfgk = get_lora_cfg(&attention_wk);
            let cfgv = get_lora_cfg(&attention_wv);
            let cfgo = get_lora_cfg(&attention_wo);
            layers.push(LayerWeights {
                attention_wq: QLoraLinear::new(
                    QMatMul::from_qtensor(attention_wq)?,
                    &cfgq,
                    lora_config,
                    vb,
                    ordering,
                    format!("model.layers.{layer_idx}.self_attn.q_proj"),
                    &mut count,
                )?,
                attention_wk: QLoraLinear::new(
                    QMatMul::from_qtensor(attention_wk)?,
                    &cfgk,
                    lora_config,
                    vb,
                    ordering,
                    format!("model.layers.{layer_idx}.self_attn.k_proj"),
                    &mut count,
                )?,
                attention_wv: QLoraLinear::new(
                    QMatMul::from_qtensor(attention_wv)?,
                    &cfgv,
                    lora_config,
                    vb,
                    ordering,
                    format!("model.layers.{layer_idx}.self_attn.v_proj"),
                    &mut count,
                )?,
                attention_wo: QLoraLinear::new(
                    QMatMul::from_qtensor(attention_wo)?,
                    &cfgo,
                    lora_config,
                    vb,
                    ordering,
                    format!("model.layers.{layer_idx}.self_attn.o_proj"),
                    &mut count,
                )?,
                attention_norm: RmsNorm::new(attention_norm, rms_norm_eps)?,
                mlp_or_moe,
                ffn_norm: RmsNorm::new(ffn_norm, rms_norm_eps)?,
                n_head: head_count,
                n_kv_head: head_count_kv,
                head_dim: embedding_length / head_count,
                cos: cos.clone(),
                sin: sin.clone(),
                span_attn,
                span_rot,
                span_mlp,
            })
        }
        let span = tracing::span!(tracing::Level::TRACE, "model");
        Ok(Self {
            tok_embeddings: Embedding::new(tok_embeddings, embedding_length),
            layers,
            norm,
            output: QMatMul::from_qtensor(output)?,
            masks: HashMap::new(),
            span,
            device: device.clone(),
            cache: Cache::new(block_count, true),
            xlora_classifier: XLoraClassifier::new(
                xlora_config,
                count,
                lora_config.len(),
                vb.clone(),
                true,
            )?,
        })
    }

    fn mask(&mut self, t: usize, device: &Device) -> Result<Tensor> {
        if let Some(mask) = self.masks.get(&t) {
            Ok(mask.clone())
        } else {
            let mask: Vec<_> = (0..t)
                .flat_map(|i| (0..t).map(move |j| u8::from(j > i)))
                .collect();
            let mask = Tensor::from_slice(&mask, (t, t), device)?;
            self.masks.insert(t, mask.clone());
            Ok(mask)
        }
    }

    fn inner_forward(
        &mut self,
        x: &Tensor,
        start_offsets: &[usize],
        scalings: Tensor,
        is_full_pass: bool,
        no_kv_cache: bool,
    ) -> Result<Tensor> {
        let (_b_sz, seq_len) = x.dims2()?;
        let mask = self.mask(seq_len, x.device())?;
        let _enter = self.span.enter();
        let mut layer_in = self.tok_embeddings.forward(x)?;
        let mut cache = if is_full_pass {
            if no_kv_cache {
                let mut new_cache = Vec::new();
                for _ in 0..self.cache.xlora_lock().len() {
                    new_cache.push(None);
                }

                *self.cache.xlora_lock() = new_cache.clone();
            }
            self.cache.xlora_lock()
        } else {
            self.cache.lock()
        };
        for (i, layer) in self.layers.iter_mut().enumerate() {
            let x = layer_in;
            let residual = &x;
            let x = layer.attention_norm.forward(&x)?;
            let attn = layer.forward_attn(
                &x,
                &mask,
                start_offsets,
                cache.get_mut(i).unwrap(),
                scalings.clone(),
                self.xlora_classifier.get_global_scaling_weight(),
            )?;
            let x = (attn + residual)?;

            // MLP
            let _enter = layer.span_mlp.enter();
            let residual = &x;
            let x = layer.ffn_norm.forward(&x)?;
            let x = layer.mlp_or_moe.forward(
                &x,
                scalings.clone(),
                self.xlora_classifier.get_global_scaling_weight(),
            )?;
            let x = (x + residual)?;
            layer_in = x
        }
        self.norm.forward(&layer_in)
    }

    pub fn forward(
        &mut self,
        input_ids: &Tensor,
        input_ids_full: &Tensor,
        seqlen_offsets: &[usize],
        seqlen_offsets_full: &[usize],
        no_kv_cache: bool,
    ) -> Result<Tensor> {
        let (b_size, seq_len_full) = input_ids_full.dims2()?;
        let (_, seq_len) = input_ids.dims2()?;

        let dummy_scalings = self.xlora_classifier.get_dummy_scalings(
            b_size,
            seq_len,
            input_ids.device(),
            DType::F32,
        )?;
        // Using X-LoRA cache here
        let hidden_states = if no_kv_cache {
            let res = self.inner_forward(
                input_ids_full,
                seqlen_offsets_full,
                dummy_scalings,
                true,
                no_kv_cache,
            )?;

            let mut new_cache = Vec::new();
            for _ in 0..self.cache.xlora_lock().len() {
                new_cache.push(Some((
                    Tensor::zeros((1,), DType::U8, &Device::Cpu)?,
                    Tensor::zeros((1,), DType::U8, &Device::Cpu)?,
                )));
            }
            *self.cache.lock() = new_cache.clone();

            res
        } else {
            self.inner_forward(
                input_ids,
                seqlen_offsets,
                dummy_scalings,
                false,
                no_kv_cache,
            )?
        };

        let scalings = self.xlora_classifier.forward(hidden_states)?;

        if no_kv_cache {
            self.inner_forward(
                input_ids_full,
                seqlen_offsets_full,
                scalings,
                true,
                no_kv_cache,
            )?
            .apply(&self.output)?
            .i((.., seq_len_full - 1, ..))
        } else {
            // is_full_pass=true is ok because no_kv_cache=false
            self.inner_forward(input_ids, seqlen_offsets, scalings, true, no_kv_cache)?
                .apply(&self.output)?
                .i((.., seq_len - 1, ..))
        }
    }
}
