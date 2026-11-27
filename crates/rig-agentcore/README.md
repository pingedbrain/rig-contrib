# rig-agentcore

Run a [Rig](https://github.com/0xPlaygrounds/rig) agent on
[AWS Bedrock AgentCore Runtime](https://docs.aws.amazon.com/bedrock-agentcore/latest/devguide/runtime-http-protocol-contract.html).

AgentCore runs an agent as an ARM64 container serving its HTTP protocol contract on port
8080. `AgentCoreApp` serves it for a Rig agent:

| Path | What it does |
|---|---|
| `POST /invocations` | Takes `{"prompt": "..."}`. Answers `{"response": "...", "status": "success"}`, or server-sent events (`{"text"}` as the model writes, then `{"response"}`) when the caller accepts `text/event-stream`. |
| `GET /ping` | `{"status": "Healthy"}`, or `"HealthyBusy"` while work is open, with `time_of_last_update`. |

```rust
use rig_agent::Agent;
use rig_agentcore::AgentCoreApp;

async fn serve(agent: Agent) -> Result<(), Box<dyn std::error::Error>> {
    AgentCoreApp::new(agent)
        .record_content(true) // prompts and replies on the spans, for AgentCore Evaluations
        .serve()              // 0.0.0.0:$PORT, default 8080; stops on SIGTERM
        .await?;
    Ok(())
}
```

- **Sessions:** the `X-Amzn-Bedrock-AgentCore-Runtime-Session-Id` header names the
  conversation; each session's messages are kept and sent with its next invocation.
  By default they live in memory (at most 1000 sessions, oldest forgotten first).
  `history_store` keeps them in any [`rig-memory`](https://crates.io/crates/rig-memory)
  store (`DirStore` on disk, `rig-sqlite`, `rig-postgres`), so they survive a restart
  and several apps can share them. Any session id works: one of 1-128 of `[A-Za-z0-9_-]`
  names its log as it is, any other is hashed into a safe log name. A save that loses
  a race with another writer retries up to 10 times; if it still fails, the answer is
  returned, the failure is logged, and that turn is not persisted. A failed load is an
  error reported before the agent runs.
  Each invocation sends the newest messages that fit `history_window` (32k tokens by
  default, `rig_memory::recent::TokenBudget`), never splitting a tool call from its
  result. An empty window sends no history. In memory, messages beyond the window are
  forgotten when a run is saved. With `history_store`, the store keeps the whole
  conversation and the window only limits what is sent.

  ```rust
  use rig_agent::Agent;
  use rig_agentcore::AgentCoreApp;

  fn sessions(agent: Agent) -> Result<AgentCoreApp, rig_memory::SessionError> {
      Ok(AgentCoreApp::new(agent)
          .history_store(rig_memory::DirStore::open("/var/lib/agent/sessions")?)
          .history_window(rig_memory::recent::TokenBudget::new(16_000)))
  }
  ```

  `DirStore` needs rig-memory's `fs` feature: enable it in your own `Cargo.toml`
  (`rig-memory = { version = "0.47", features = ["fs"] }`).
- **Summaries:** `summarize_history(summarizer, keep_recent, trigger_messages)` makes a
  long session shorter. Once a session holds more than `trigger_messages` messages, an
  agent without tools (`summarizer`) summarizes all but the newest `keep_recent`, and that
  one summary becomes the session's first message. The cut never separates a tool call
  from its result. If the summary fails, a warning is logged, the session keeps its full
  history, and the invocation answers as usual. With `history_store`, the summary is written
  to the store in place of the messages it replaces. Rig-memory has no summarizing window
  (its compaction is a planning API the caller drives), so this option is built on its
  compaction helpers.

  Each summary attempt is traced as an `agentcore.summarize` span under the invocation span.
  It records the messages summarized and kept, the outcome (`summarized`, `skipped_changed`,
  `skipped_nothing_older` or `failed`), and the summarizer's token usage. The prompt and the
  summary text are recorded only with `record_content`. Success is also an info event, and a
  failure keeps its warning.

  ```rust
  use rig_agent::Agent;
  use rig_agentcore::AgentCoreApp;

  fn app(agent: Agent, summarizer: Agent) -> AgentCoreApp {
      AgentCoreApp::new(agent).summarize_history(summarizer, 20, 60)
  }
  ```
- **Traces:** each invocation runs in an `agentcore.invocation` span that continues the
  caller's trace (`traceparent`) and gives every span under it `session.id`, through
  [`rig-otel`](https://crates.io/crates/rig-otel). With rig-otel's `aws` feature exporting to CloudWatch, that
  is what AgentCore Observability and AgentCore Evaluations read.
- **Limits:** all opt-in. `max_concurrent_tasks` caps invocations running at once (past
  the cap, 429 before the agent runs; no cap by default, since AgentCore Runtime gives each
  session its own microVM). `max_run_duration` stops an invocation, summarizing included,
  after a time (504, or an `{"error"}` event when streaming, and its turn is not kept; no
  limit by default). `max_request_bytes` sets the body limit (default 2 MB, answered with 413).
  Outside AgentCore Runtime, a cap around 32 and a ten-minute run limit suit most agents.

  ```rust
  use std::time::Duration;

  use rig_agent::Agent;
  use rig_agentcore::AgentCoreApp;

  fn limited(agent: Agent) -> AgentCoreApp {
      AgentCoreApp::new(agent)
          .max_concurrent_tasks(32)
          .max_run_duration(Duration::from_secs(600))
          .max_request_bytes(256 * 1024)
  }
  ```
- **Background work:** `/ping` answers `HealthyBusy` while an invocation is running, and
  while work from `spawn_task` or a `track_task` guard is open. AgentCore Runtime keeps a
  session running while it reports `HealthyBusy`, so a background task outlives the
  invocation that started it. The status returns to `Healthy` when the work ends.

  ```rust
  use rig_agentcore::AgentCoreApp;

  fn start_background(app: &AgentCoreApp) {
      app.spawn_task(async {
          // Work that continues after the invocation has answered.
      });
  }
  ```
- **Content:** `record_content(true)` records prompts and replies on the GenAI spans.
  AgentCore Evaluations needs them to judge a session. They are off by default, as in Rig.
  The setting applies to every invocation and always overrides `AgentBuilder::record_content_telemetry`
  on the agent. The agent's setting is ignored when it is served through `AgentCoreApp`, so to
  record content, call `.record_content(true)` on the app. This is planned to change in 0.48
  (PR #42), so that an app setting left unset keeps the agent's.

## Verified on AWS

On 2026-11-26, an agent built with this crate ran on Amazon Bedrock AgentCore Runtime in
us-east-1:

- It was deployed as an ARM64 container image from Amazon ECR, with an execution role.
- A two-turn session kept its conversation across invocations.
- Its traces reached CloudWatch (`aws/spans`), with the session id on every span. The agent
  used `record_content(true)` and rig-otel's `aws` export.
- AgentCore Evaluations' built-in evaluators scored the sessions from those spans:
  GoalSuccessRate 1.0, Helpfulness 0.83 and Correctness 1.0 per turn.

That agent was a private application that used a calculator tool and a Bedrock model. The
steps below use the public `serve` example instead. They reproduce the deployment and
invocation. The example does not export traces, so step 6 adds that to your own agent.

Set these once. Replace the placeholders with your values:

```sh
export AWS_REGION=<region>
export ACCOUNT=<account>
export REPO=rig-agentcore-serve
export IMAGE=$ACCOUNT.dkr.ecr.$AWS_REGION.amazonaws.com/$REPO:latest
```

1. Build the ARM64 image from the repository root. Docker needs buildx. On an x86 host the
   build runs under emulation and is slow.

   ```sh
   docker buildx build --platform linux/arm64 \
     -f crates/rig-agentcore/deploy/Dockerfile -t $IMAGE --load .
   ```

2. Create an ECR repository and push the image:

   ```sh
   aws ecr create-repository --repository-name $REPO \
     --image-scanning-configuration scanOnPush=true --region $AWS_REGION
   aws ecr get-login-password --region $AWS_REGION | \
     docker login --username AWS --password-stdin $ACCOUNT.dkr.ecr.$AWS_REGION.amazonaws.com
   docker push $IMAGE
   ```

3. Create an execution role for the runtime. AWS documents the required permissions in
   [IAM permissions for AgentCore Runtime](https://docs.aws.amazon.com/bedrock-agentcore/latest/devguide/runtime-permissions.html).
   Save the trust policy as `trust.json`:

   ```json
   {
     "Version": "2012-10-17",
     "Statement": [{
       "Effect": "Allow",
       "Principal": {"Service": "bedrock-agentcore.amazonaws.com"},
       "Action": "sts:AssumeRole",
       "Condition": {
         "StringEquals": {"aws:SourceAccount": "<account>"},
         "ArnLike": {"aws:SourceArn": "arn:aws:bedrock-agentcore:<region>:<account>:*"}
       }
     }]
   }
   ```

   Save this permissions policy as `execution.json`. It covers pulling the image, writing logs,
   and sending traces and metrics (the statements AWS lists for the runtime execution role).
   Add `bedrock:InvokeModel` and `bedrock:InvokeModelWithResponseStream` for your model
   only if your agent calls Bedrock:

   ```json
   {
     "Version": "2012-10-17",
     "Statement": [
       {"Effect": "Allow", "Action": ["ecr:BatchGetImage", "ecr:GetDownloadUrlForLayer"],
        "Resource": "arn:aws:ecr:<region>:<account>:repository/<repo>"},
       {"Effect": "Allow", "Action": "ecr:GetAuthorizationToken", "Resource": "*"},
       {"Effect": "Allow", "Action": ["logs:CreateLogGroup", "logs:DescribeLogStreams"],
        "Resource": "arn:aws:logs:<region>:<account>:log-group:/aws/bedrock-agentcore/runtimes/*"},
       {"Effect": "Allow", "Action": "logs:DescribeLogGroups",
        "Resource": "arn:aws:logs:<region>:<account>:log-group:*"},
       {"Effect": "Allow", "Action": ["logs:CreateLogStream", "logs:PutLogEvents"],
        "Resource": "arn:aws:logs:<region>:<account>:log-group:/aws/bedrock-agentcore/runtimes/*:log-stream:*"},
       {"Effect": "Allow",
        "Action": ["xray:PutTraceSegments", "xray:PutTelemetryRecords",
                   "xray:GetSamplingRules", "xray:GetSamplingTargets"],
        "Resource": "*"},
       {"Effect": "Allow", "Action": "cloudwatch:PutMetricData", "Resource": "*",
        "Condition": {"StringEquals": {"cloudwatch:namespace": "bedrock-agentcore"}}}
     ]
   }
   ```

   ```sh
   aws iam create-role --role-name <role-name> --assume-role-policy-document file://trust.json
   aws iam put-role-policy --role-name <role-name> --policy-name execution \
     --policy-document file://execution.json
   export ROLE_ARN=$(aws iam get-role --role-name <role-name> --query Role.Arn --output text)
   ```

4. Create the runtime with the AWS CLI. Its `help` output lists every option:
   `aws bedrock-agentcore-control create-agent-runtime help`. Or use the console, under
   Amazon Bedrock AgentCore. The developer guide is at
   <https://docs.aws.amazon.com/bedrock-agentcore/latest/devguide/>.

   ```sh
   aws bedrock-agentcore-control create-agent-runtime \
     --agent-runtime-name rig_agentcore_serve \
     --agent-runtime-artifact "{\"containerConfiguration\":{\"containerUri\":\"$IMAGE\"}}" \
     --role-arn $ROLE_ARN \
     --network-configuration networkMode=PUBLIC \
     --protocol-configuration serverProtocol=HTTP \
     --region $AWS_REGION
   ```

   The runtime starts in `CREATING`. Wait until `status` is `READY`, then note its ARN:

   ```sh
   aws bedrock-agentcore-control get-agent-runtime --agent-runtime-id <runtime-id> \
     --query '{status:status,arn:agentRuntimeArn}' --region $AWS_REGION
   ```

   The example serves `/ping` and `/invocations` on port 8080, as the
   [HTTP protocol contract](https://docs.aws.amazon.com/bedrock-agentcore/latest/devguide/runtime-http-protocol-contract.html)
   requires.

5. Invoke the runtime. The session id must be at least 33 characters, and `uuidgen` gives 36.
   The output goes to `out.json`:

   Save the request body as `payload.json`:

   ```sh
   echo '{"prompt":"Hi, I am Ana."}' > payload.json
   ```

   ```sh
   aws bedrock-agentcore invoke-agent-runtime \
     --agent-runtime-arn <agent-runtime-arn> \
     --runtime-session-id "$(uuidgen)" \
     --content-type application/json --accept application/json \
     --payload fileb://payload.json \
     --region $AWS_REGION out.json
   cat out.json
   ```

   Send a second call with the same `--runtime-session-id` and a prompt such as
   `What is my name?`. The runtime's logs show that the second request carries the first
   turn's messages. Without `OPENROUTER_MODEL` and `OPENROUTER_API_KEY` in the runtime's
   environment, the example answers with a scripted model. Keep API keys out of plain
   environment variables: use AWS Secrets Manager.

6. Enable observability. Turn on CloudWatch Transaction Search once per account. AWS
   documents the steps in
   [Enable transaction search](https://docs.aws.amazon.com/AmazonCloudWatch/latest/monitoring/Enable-TransactionSearch.html).
   Create the resource policy from that page (`aws logs put-resource-policy`), then run:

   ```sh
   aws xray update-trace-segment-destination --destination CloudWatchLogs --region $AWS_REGION
   ```

   Then add traces to your agent. Build the agent with `record_content(true)`, call
   `rig_otel::Telemetry::builder().aws_cloudwatch().await`, and set
   `OTEL_RESOURCE_ATTRIBUTES=service.name=<name>,aws.log.group.names=/aws/bedrock-agentcore/runtimes/<runtime-id>-DEFAULT`
   on the runtime, as `rig-otel` documents. AgentCore Observability and Evaluations then read
   the spans.

7. Clean up when you are done:

   ```sh
   aws bedrock-agentcore-control delete-agent-runtime --agent-runtime-id <runtime-id> --region $AWS_REGION
   aws ecr delete-repository --repository-name $REPO --force --region $AWS_REGION
   aws iam delete-role-policy --role-name <role-name> --policy-name execution
   aws iam delete-role --role-name <role-name>
   ```

## Testing

`cargo test` serves the contract over real HTTP for agents on scripted models: ping (with
busy status while work is open), a
session keeping its conversation, sessions kept apart, streamed replies, a bad body,
the run and body limits, and an invocation continuing the caller's trace with the session on every span.
