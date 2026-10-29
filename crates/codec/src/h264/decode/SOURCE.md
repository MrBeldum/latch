The scalar reconstruction kernels were adapted from pith-h264 0.1.1
(https://github.com/pith-hash/pith-h264), distributed under the MIT license
in LICENSE.pith-h264. The source archive is pinned by SHA-256:
d9981626467b304efb1be5abd3a84965fa3d344eea48c84d665706dbce426c6c

Adapted:
- Scalar reconstruction kernels for Constrained Baseline Profile (ITU-T H.264).
- Intra 4x4, Intra 16x16, and chroma prediction modes.
- 4x4 inverse integer transform and Hadamard DC transforms.
- CAVLC coefficient and syntax decoding.
- Inter prediction motion compensation for P macroblocks (16x16, 16x8, 8x16, 8x8, 8x4, 4x8, 4x4).
- In-loop deblocking filter.
- Decoded Picture Buffer (DPB) reference management and sliding-window / MMCO marking.

Removed:
- 8x8 transform, 8x8 inverse DCT, 8x8 dequantisation, and Intra 8x8 prediction modes.
- B-slice syntax, bidirectional prediction, spatial direct, and temporal direct prediction colocation tracking.
- CABAC entropy decoding, contexts, and tables.
- Upstream FFI bindings, benchmarking harness, and SHA/hashing integration.
