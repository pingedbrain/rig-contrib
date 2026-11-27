# The `serve` example as an ARM64 container for AgentCore Runtime.
# Build from the repository root (the build context must contain the workspace):
#
#   docker buildx build --platform linux/arm64 \
#     -f crates/rig-agentcore/deploy/Dockerfile -t rig-agentcore-serve:latest --load .
#
# Without OPENROUTER_MODEL and OPENROUTER_API_KEY the example answers with a scripted model.
FROM rust:1.100-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p rig-agentcore --example serve

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/examples/serve /serve
ENV PORT=8080
EXPOSE 8080
ENTRYPOINT ["/serve"]
