//! Systematic Reed–Solomon erasure coding over GF(2⁸), polynomial 0x11D.
//!
//! A block has `data` data shards and `parity` parity shards, all the same
//! length, `data + parity <= 255`. Parity shard `i` is
//! `Σ_j C[i][j] · data[j]`, where `C[i][j] = 1 / (x_i ⊕ y_j)` with
//! `x_i = data + i` and `y_j = j` (a Cauchy matrix: every square submatrix
//! is invertible, so any `data` shards of a block rebuild it). Host and
//! viewer must compute exactly this matrix; it is part of the protocol.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FecError {
    /// `data` is 0, or `data + parity` is over 255.
    Shape,
    /// Shards of different lengths.
    Length,
    /// Fewer than `data` shards present.
    TooFewShards,
}

impl std::fmt::Display for FecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FecError::Shape => write!(f, "bad FEC block shape"),
            FecError::Length => write!(f, "FEC shards differ in length"),
            FecError::TooFewShards => write!(f, "too few shards to rebuild the FEC block"),
        }
    }
}

impl std::error::Error for FecError {}

/// The coder for one block shape. Cheap to build; [`ReedSolomon::cached`]
/// keeps the recently used shapes.
pub struct ReedSolomon {
    data: usize,
    parity: usize,
}

impl ReedSolomon {
    pub fn new(data: usize, parity: usize) -> Result<Self, FecError> {
        let _ = (data, parity);
        todo!()
    }

    /// A shared coder for this shape from a small cache.
    pub fn cached(data: usize, parity: usize) -> Result<std::sync::Arc<Self>, FecError> {
        let _ = (data, parity);
        todo!()
    }

    pub fn data(&self) -> usize {
        self.data
    }

    pub fn parity(&self) -> usize {
        self.parity
    }

    /// Fills `parity` from `data`. Every slice has the same length.
    pub fn encode(&self, data: &[&[u8]], parity: &mut [&mut [u8]]) -> Result<(), FecError> {
        let _ = (data, parity);
        todo!()
    }

    /// `shards` holds the block's `data + parity` shards in order, `None`
    /// for each missing one. Rebuilds every missing data shard in place
    /// (missing parity shards may stay `None`).
    pub fn reconstruct(&self, shards: &mut [Option<Vec<u8>>]) -> Result<(), FecError> {
        let _ = shards;
        todo!()
    }
}
