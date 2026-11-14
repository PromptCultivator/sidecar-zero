//! Native fallbacks (sdk-python §4): descriptions evaluated in Rust, with no Python call.

use crate::input::DenseIn;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Link {
    Identity,
    Sigmoid,
}

pub enum Kind {
    /// Any value, returned as is, for any codec.
    Constant(Py<PyAny>),
    /// `then` if feature > gt, else `otherwise`.
    Threshold { feature: usize, gt: f64, then: f64, otherwise: f64 },
    /// link(w · x + bias).
    Linear { weights: Vec<f64>, bias: f64, link: Link },
    /// table[floor(x[feature])], or `otherwise` outside the table.
    Lookup { feature: usize, table: Vec<f64>, otherwise: f64 },
}

/// A fallback the core evaluates itself. Build one with `sidecarzero.fallbacks`.
#[pyclass(frozen, module = "sidecarzero.fallbacks", name = "NativeFallback")]
pub struct NativeFallback {
    pub kind: Kind,
}

impl NativeFallback {
    /// Construction-time check against the codec (sdk-python §4): None for a constant, or the
    /// input width it needs.
    pub fn check(&self, dense_n: Option<usize>) -> PyResult<()> {
        let need = |what: &str| PyValueError::new_err(format!("a {what} fallback needs the dense codec"));
        match &self.kind {
            Kind::Constant(_) => Ok(()),
            Kind::Threshold { feature, .. } | Kind::Lookup { feature, .. } => {
                let n = dense_n.ok_or_else(|| need("threshold or lookup"))?;
                if *feature >= n {
                    return Err(PyValueError::new_err(format!("feature {feature} is beyond the input's {n} features")));
                }
                Ok(())
            }
            Kind::Linear { weights, .. } => {
                let n = dense_n.ok_or_else(|| need("linear"))?;
                if weights.len() != n {
                    return Err(PyValueError::new_err(format!("{} weights for an input of {n} features", weights.len())));
                }
                Ok(())
            }
        }
    }

    /// The score for row `r` of an input the codec accepted; for one it refused, the value a
    /// row with no information gets.
    pub fn score(&self, x: &DenseIn, r: usize) -> f32 {
        let ok = x.ok && r < x.rows as usize;
        match &self.kind {
            Kind::Constant(_) => f32::NAN,
            Kind::Threshold { feature, gt, then, otherwise } => {
                if ok && x.get(r, *feature) > *gt {
                    *then as f32
                } else {
                    *otherwise as f32
                }
            }
            Kind::Linear { weights, bias, link } => {
                let mut z = *bias;
                if ok {
                    for (c, w) in weights.iter().enumerate() {
                        z += w * x.get(r, c);
                    }
                }
                match link {
                    Link::Identity => z as f32,
                    Link::Sigmoid => (1.0 / (1.0 + (-z).exp())) as f32,
                }
            }
            Kind::Lookup { feature, table, otherwise } => {
                if !ok {
                    return *otherwise as f32;
                }
                let v = x.get(r, *feature).floor();
                if v >= 0.0 && (v as usize) < table.len() {
                    table[v as usize] as f32
                } else {
                    *otherwise as f32
                }
            }
        }
    }

    pub fn constant(&self) -> Option<&Py<PyAny>> {
        match &self.kind {
            Kind::Constant(v) => Some(v),
            _ => None,
        }
    }
}

#[pymethods]
impl NativeFallback {
    /// The check a client built with `codec` makes (ValueError if they do not fit).
    fn _check(&self, codec: &Bound<'_, PyAny>) -> PyResult<()> {
        self.check(codec.cast::<crate::codecs::DenseF32ToScore>().ok().map(|c| c.get().n))
    }
    /// Evaluate for `input` as a client with `codec` would (for the in-process test double).
    fn _apply(&self, py: Python<'_>, codec: &Bound<'_, PyAny>, input: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        if let Some(v) = self.constant() {
            return Ok(v.clone_ref(py));
        }
        let c = codec.cast::<crate::codecs::DenseF32ToScore>()?.get();
        self.check(Some(c.n))?;
        let (e, d, _) = crate::input::dense(input, c.n, c.strict);
        let v: Vec<f32> = if d.ok { (0..d.rows as usize).map(|r| self.score(&d, r)).collect() } else { vec![self.score(&d, 0)] };
        drop(e);
        if !d.ok || d.scalar {
            return Ok(pyo3::types::PyFloat::new(py, v[0] as f64).into_any().unbind());
        }
        crate::client::f32_array(py, &v)
    }
    fn __repr__(&self, py: Python<'_>) -> PyResult<String> {
        Ok(match &self.kind {
            Kind::Constant(v) => format!("constant({})", v.bind(py).repr()?),
            Kind::Threshold { feature, gt, then, otherwise } => format!("threshold(feature={feature}, gt={gt}, then={then}, otherwise={otherwise})"),
            Kind::Linear { weights, bias, link } => format!("linear(weights=<{}>, bias={bias}, link={:?})", weights.len(), format!("{link:?}").to_lowercase()),
            Kind::Lookup { feature, table, otherwise } => format!("lookup(feature={feature}, table=<{}>, otherwise={otherwise})", table.len()),
        })
    }
}

#[pyfunction]
pub fn constant(value: Py<PyAny>) -> NativeFallback {
    NativeFallback { kind: Kind::Constant(value) }
}

#[pyfunction]
#[pyo3(signature = (*, feature, gt, then, otherwise))]
pub fn threshold(feature: usize, gt: f64, then: f64, otherwise: f64) -> NativeFallback {
    NativeFallback { kind: Kind::Threshold { feature, gt, then, otherwise } }
}

#[pyfunction]
#[pyo3(signature = (*, weights, bias = 0.0, link = "identity"))]
pub fn linear(weights: Vec<f64>, bias: f64, link: &str) -> PyResult<NativeFallback> {
    let link = match link {
        "identity" => Link::Identity,
        "sigmoid" => Link::Sigmoid,
        _ => return Err(PyValueError::new_err(format!("link must be identity or sigmoid, not {link:?}"))),
    };
    if weights.is_empty() {
        return Err(PyValueError::new_err("a linear fallback needs weights"));
    }
    Ok(NativeFallback { kind: Kind::Linear { weights, bias, link } })
}

#[pyfunction]
#[pyo3(signature = (*, feature, table, otherwise))]
pub fn lookup(feature: usize, table: Vec<f64>, otherwise: f64) -> NativeFallback {
    NativeFallback { kind: Kind::Lookup { feature, table, otherwise } }
}
