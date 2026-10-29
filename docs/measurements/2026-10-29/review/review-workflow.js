export const meta = {
  name: 'latch5-engine-review',
  description: 'Four independent area reviews of the Latch 5 Rust engine, then one adversarial verifier',
  phases: [
    { title: 'Review', detail: 'protocol, session lifecycle, security/host, platform pipelines' },
    { title: 'Verify', detail: 'one skeptic tries to refute every finding' },
  ],
}

const REPO = '/Users/daniel/Desktop/dev/latch-int'
const COMMON = `You are reviewing Latch 5, a desktop-streaming app whose streaming engine was just rewritten in Rust (replacing Sunshine and moonlight-common-c). The code is in the git worktree ${REPO} (branch rewrite/integration). Read docs/ENGINE.md and docs/PROTOCOL.md first: they are the intended behaviour. Do NOT edit any file. You may run read-only commands and cargo (export PATH=$HOME/.cargo/bin:$PATH TMPDIR=/tmp first; never run cargo clean). Report only real defects you can point to in the code with a concrete failure scenario (inputs/state -> wrong result, crash, hang, leak, security hole). No style nits, no speculative "consider". Rank by severity. For each: file, line, a one-sentence defect, the concrete failure scenario, and the fix you would make.`

const FINDINGS = {
  type: 'object',
  properties: {
    findings: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          severity: { type: 'string', enum: ['critical', 'high', 'medium', 'low'] },
          file: { type: 'string' },
          line: { type: 'integer' },
          defect: { type: 'string' },
          scenario: { type: 'string' },
          fix: { type: 'string' },
        },
        required: ['severity', 'file', 'line', 'defect', 'scenario', 'fix'],
      },
    },
  },
  required: ['findings'],
}

const AREAS = [
  { key: 'protocol', prompt: `${COMMON}\n\nYOUR AREA: crates/proto (wire.rs, fec.rs incl. its std::arch NEON/x86 paths vs the scalar path, video.rs Packetizer/Reassembler/KeyframeRequests, audio.rs, input.rs InputSender/InputReceiver, control.rs, rtt.rs) against docs/PROTOCOL.md. Look for: spec mismatches between packetizer and reassembler (block split, parity counts, shard_len, empty frames), wrap-around of u32 frame/seq numbers, frames delivered out of order or twice, loss never declared (hangs) or declared wrongly, keyframe gating mistakes, unbounded memory from a malicious or broken peer, panics on malformed datagrams, input events applied twice/out of order/lost, audio parity reconstruction errors, control framing that a peer can abuse.` },
  { key: 'lifecycle', prompt: `${COMMON}\n\nYOUR AREA: session lifecycle and concurrency on both ends: crates/engine/src/server.rs (Server::bind/prepare/run/control, the UDP receive thread, send_video, send_audio, takeover, stop, Prepared drop), crates/stream/src/session.rs (Session::start/run/control_loop/receive/decode/stop, Input), and crates/core/src/http.rs upgrade()/switch()/serve_with_peer_check. Look for: deadlocks (lock ordering, holding a lock across blocking calls), threads that never exit (blocking reads without timeouts, channels never closed), a stopped session that keeps capturing or keeps the display prepared, takeover races (two prepares, run installing a session while another runs), keepalive/silence timeouts that fire wrongly or never, a viewer that never gets Connected/Failed/Terminated, keyframe-request storms or starvation, stats that are wrong, resource leaks per session (sockets, threads), behaviour when the viewer's UDP never arrives, when TCP closes first, when the decoder is slow.` },
  { key: 'security', prompt: `${COMMON}\n\nYOUR AREA: security and host integration: crates/host/src/service.rs (the POST /v1/stream route, caller()/authorized(), the HTTP upgrade path, status, update handover), crates/host/src/prep.rs (prep commands run with environment variables, the sunshine.conf migration parser), crates/host/src/setup.rs and crates/host/src/legacy.rs (the generated elevated Windows PowerShell: firewall rules, retiring Latch 4's engine), crates/host/src/unix_setup.rs, crates/engine/src/server.rs's UDP handling (session token, address binding, what an unauthenticated tailnet peer or a local process can do by sending datagrams to port 47850), and deploy/ (Dockerfile, native units, migration script). Look for: authorization bypasses (a peer not on the owner's Tailscale account starting or hijacking a session, injecting input, reading video), command injection or path traversal in generated scripts or prep commands, the userspace-Tailscale (loopback) case, denial of service by datagram floods or by holding the single session slot, elevated script steps that could delete the wrong things, secrets in logs.` },
  { key: 'platform', prompt: `${COMMON}\n\nYOUR AREA: the platform pipelines: crates/engine/src/software.rs, crates/engine/src/linux/**, crates/engine/src/mac/**, crates/engine/src/windows/**, crates/stream/src/video/{videotoolbox.rs,mediafoundation.rs,software.rs} and crates/stream/src/video.rs. Look for: unsound unsafe code (lifetimes of COM/CF/ObjC objects, buffer sizes, aliasing), leaks per frame, wrong VideoPipeline::next semantics (idle repeat, forced keyframe within one frame interval, size changes), wrong colour range/matrix, input injection bugs (stuck keys, wrong coordinates, modifiers), audio format conversions. IMPORTANT: GitHub's Windows CI runner reported that the Media Foundation software round trip test (windows::tests::software_mf_round_trip in crates/engine/src/windows) produced 0 decoded pictures: the software H.264 encoder MFT's output fed to MfDecoder (crates/stream/src/video/mediafoundation.rs) gave no picture. Find the root cause by reading both the encoder path and MfDecoder (input/output types, MFT_MESSAGE notifications, MF_E_TRANSFORM_NEED_MORE_INPUT / STREAM_CHANGE handling, sample timestamps/durations, Annex B vs length-prefixed input, low-latency attributes, draining) and report it as a finding with the exact fix.` },
]

phase('Review')
const reviews = await parallel(AREAS.map(a => () =>
  agent(a.prompt, { label: `review:${a.key}`, phase: 'Review', schema: FINDINGS })
    .then(r => (r ? r.findings.map(f => ({ ...f, area: a.key })) : []))
))
const all = reviews.filter(Boolean).flat()
log(`${all.length} findings from ${AREAS.length} reviewers`)
if (all.length === 0) return { confirmed: [], refuted: [] }

phase('Verify')
const VERDICTS = {
  type: 'object',
  properties: {
    verdicts: {
      type: 'array',
      items: {
        type: 'object',
        properties: {
          index: { type: 'integer' },
          real: { type: 'boolean' },
          reason: { type: 'string' },
        },
        required: ['index', 'real', 'reason'],
      },
    },
  },
  required: ['verdicts'],
}
const listing = all.map((f, i) => `[${i}] (${f.severity}, ${f.area}) ${f.file}:${f.line} — ${f.defect}\n    scenario: ${f.scenario}\n    fix: ${f.fix}`).join('\n\n')
const verdicts = await agent(`${COMMON}\n\nYOUR JOB: adversarially verify these findings from other reviewers. For EACH one, open the code and try to refute it: is the scenario actually reachable with this code? Does some other check already prevent it? Is the claimed behaviour really wrong per docs/PROTOCOL.md / docs/ENGINE.md? Mark real=true only if you confirmed the defect yourself in the code; when uncertain, real=false with your reason. Return one verdict per index.\n\nFINDINGS:\n\n${listing}`, { label: 'verify', phase: 'Verify', schema: VERDICTS })
const byIndex = new Map((verdicts?.verdicts || []).map(v => [v.index, v]))
const confirmed = [], refuted = []
all.forEach((f, i) => {
  const v = byIndex.get(i)
  if (v && v.real) confirmed.push({ ...f, verdict: v.reason })
  else refuted.push({ ...f, verdict: v ? v.reason : 'no verdict' })
})
log(`${confirmed.length} confirmed, ${refuted.length} refuted or unverified`)
return { confirmed, refuted }
