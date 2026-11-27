# Changelog

Versions follow Rig's: rig-agentcore 0.47.x works with Rig 0.47.

## Unreleased

- `AgentCoreApp::max_history_tokens`: each session keeps at most 32,000 tokens of
  history by default, the newest messages; before, a session grew without bound.

## 0.47.2 (2026-11-26)

- docs.rs documents every feature, each item marked with the feature it needs.
- Links between the crates' READMEs point to crates.io, so they work there.

## 0.47.0 (2026-11-26)

First release.

- `AgentCoreApp`: AgentCore Runtime's HTTP contract for a Rig agent, `/invocations` in
  JSON or server-sent events and `/ping`; a conversation per runtime session; spans that
  continue the caller's trace and carry `session.id`; optional content recording for
  AgentCore Evaluations.
