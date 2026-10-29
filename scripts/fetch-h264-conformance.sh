#!/bin/sh
# ITU H.264.1 AVCv1 archives, SHA-256 pinned from the published downloads.
# Readmes remain alongside the original bitstream and decoded YUV.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
dest="$root/crates/codec/tests/conformance"
mkdir -p "$dest/.archives"
base=https://www.itu.int/wftp3/av-arch/jvt-site/draft_conformance/AVCv1
while read -r sha name; do
    archive="$dest/.archives/$name.zip"
    if [ ! -f "$archive" ]; then
        curl --fail --location --retry 3 "$base/$name.zip" -o "$archive.partial"
        mv "$archive.partial" "$archive"
    fi
    printf '%s  %s\n' "$sha" "$archive" | shasum -a 256 --check --status
    mkdir -p "$dest/$name"
    # Only original media and readmes; trace logs are enormous.
    unzip -oq "$archive" '*.264' '*.h264' '*.jsv' '*.yuv' '*.qcif' '*readme*' '*Readme*' -d "$dest/$name" 2>/dev/null || [ "$?" -eq 11 ]
done <<'MANIFEST'
ecaed4a05d02c8ffc0ec7dd02a640f87ba9d9f8c2dab3273ef1d6da3d7f8568e AUD_MW_E
f21e1b0b7b820ac6f078bc5e9aa3284e068d2fdf1192e6c5c42509a070b8bb84 BA1_FT_C
399eae2eb03f82fc291bdc4fe9cedd110b40a96368ffe1e1bcf9ff6436fa9df7 BA1_Sony_D
7ee27c0879fd43d3203cc14d5f2381e737bfb287fc6bdc4b64ff77f16c574397 BA2_Sony_F
4d37b02cd96ce57f91db306bcd29976ac26b88efbc1c8b3322101f7914378303 BAMQ1_JVC_C
eef3aba4a9c99a4ea61ec35aab77246a1c4ff3249588e1e3fb153e80bc05d256 BAMQ2_JVC_C
b50e2e7ca2a355087e9d993174626a027cea2da5f84f704cc0b2c6cb9145dfbd BANM_MW_D
f7cdbfaee43ff2e88b78d0b41ddcde83ac5148867e7b7df1759d97e0294f6c19 BASQP1_Sony_C
f85d0c1e6a8317ecff19bf11d1496b327ee11e391de27453031062ea62241843 BA_MW_D
b44b82586b4be8232d77e7d0971691004f7aed5ef7d8730b20f81405fbc77d1a CI1_FT_B
6e41825983037e3af50a3e4add6ac9dbde3ea6f41ab6f77cc432de22a2920d49 CI_MW_D
b34a28852630abe770b680dbd7e0c619a01eacfffd2d928ab4aff3bc76746796 LS_SVA_D
69e7d04bd4b782f93401b12b79d58a96b39319d932743af8bc7134c752d6b577 MIDR_MW_D
dd96a4e890224097f4f7d8e4c3b47aba3bb986e86b6374d6a50af44453841575 MPS_MW_A
a76d4e199e6ffb27292d1a07c516e5adea251e63e3f756de490c153b23d4684d MR1_BT_A
63171ed2251df09bee5560db87cc063e7a54f1602596734d03779a6239bc09d8 MR1_MW_A
97e703fef42704e5e91dc96d92416790f38b4df0a3f97b02c138f9def4e87325 MR2_MW_A
a1fef6ee3e9709360033cc5d0a1a39a36827c4a28f4fc9693904db5a87e5a374 MR2_TANDBERG_E
007ed664447935137a5789af31d6c6c37af2cae2b98b9cf047c936dde698f068 MR3_TANDBERG_B
a1012779b3d6a60a0459d1961025194c1c016ee31f012f5014c076de78b09ecb MR4_TANDBERG_C
fb86b6f6512d14fbfe1a369e1cfa230a87a2e2d630fa7762dfc3b33e3b219865 MR5_TANDBERG_C
1b26bfe8d05cf54b95e410779e48f90cc55305186e0c6184b5ea28ce9dc1b02d NL1_Sony_D
201e351e9c9680d514029cf4ec11a3009368d6955ec082cff9c244c6c03b818c NL2_Sony_H
079cebd9a12e9569164bffe91dd2ed830dc8cd25186652530d9a2622f5ddc398 NLMQ1_JVC_C
35ce821ebf67b4a4c706df90fc1a3a27a86b0d0fc3cd5fcc52624ef352b381f1 NLMQ2_JVC_C
c48f067fe8ac1309b99292d024324a0a7561bf28312a133995a42d7a98ae9835 NRF_MW_E
1d19e5339d677ba6920912e7ca2d0a2889ff61fe0c4bbf0167da371fa855061b SVA_BA1_B
e0cefee7e9206d06cad7bbdbe1441b6566208d693bcebc4396fa896be3a2c8d4 SVA_BA2_D
0ab8582893c671db242d2a645745a96b45bd4e479ccce8b0a6d989ee87e259a7 SVA_Base_B
636098f10f2cdc80e31451b198c688bcc868fe536707c46ce2e0eaf54a33fa98 SVA_CL1_E
9db49088a67d960d1363605c40a603d38c87361f3b83e730e6541c5a1db4c10f SVA_FM1_E
571504481df461f11c4f028df8754d06d1f5dc24171c571b9b4808b0cb595dbf SVA_NL1_B
f21e06d253b205aa65e796158daecfbcfc4ea2b5c41af1412f6bdd54a0a59977 SVA_NL2_E
99aa2f1501cd1ca8af1a292bb1f1f1cb49e6189a3131bc5bc8168b3dd322905a CVFC1_Sony_C
4d7d78a590acaf36f197d746c122281f3c0629abcb964471215b17e8c39f41d8 CVPCMNL1_SVA_C
90b0574aa7d02f8b250c4997b78fd4f112983fd3eb9eb7c12df6d5b7560ceb1d CVPCMNL2_SVA_C
9038147c4300ae373c07c41d0a172ac593339978f36470a2561e6b4c5da65645 HCBP1_HHI_A
9847f5d4941ac05c0736f9777398005d7fa72faea6f283a18e2fa72c8ea42659 HCBP2_HHI_A
MANIFEST
# Deliberate exclusions after inspecting the archive readmes:
# BA3_SVA_C, NL3_SVA_E, SL1_SVA_B: B slices.
# FM1_BT_B, FM1_FT_E, FM2_SVA_C: FMO.
# MR6_BT_B, MR7_BT_B: field/MBAFF; MR8_BT_B, MR9_BT_B: fields and B slices.
# Other AVCv1 CA*/CV*/FI*/SP*/HC*/MV* streams use CABAC, B slices,
# fields, weighted prediction, SP/SI, or non-Baseline extensions.
printf 'Installed %s; run cargo test -p latch-codec --test conformance -- --nocapture\n' "$dest"
