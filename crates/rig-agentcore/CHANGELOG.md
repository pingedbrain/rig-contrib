# Changelog

Versions follow Rig's: rig-agentcore 0.47.x works with Rig 0.47.

## 0.47.0 (unreleased)

First release.

- `AgentCoreApp`: AgentCore Runtime's HTTP contract for a Rig agent, `/invocations` in
  JSON or server-sent events and `/ping`; a conversation per runtime session; spans that
  continue the caller's trace and carry `session.id`; optional content recording for
  AgentCore Evaluations.
