# syntax=docker/dockerfile:1
# ---------------------------------------------------------------------------
# sempkg-agent server image.
#
# The agent shells out to the `sempkg` MCP server for retrieval, so the image
# needs the `sempkg` binary on PATH and a synced workspace (sempkg.toml + the
# installed bundles under .sempkg/). Provide those by either:
#   * COPY-ing a prebuilt `sempkg` binary into /usr/local/bin (uncomment below), or
#   * mounting them at runtime (-v /path/sempkg:/usr/local/bin/sempkg
#                               -v /path/workspace:/workspace).
# ---------------------------------------------------------------------------
FROM python:3.11-slim AS base

ENV PYTHONUNBUFFERED=1 \
    PIP_NO_CACHE_DIR=1

WORKDIR /app

# uv for fast, reproducible installs (matches the repo's tooling).
COPY --from=ghcr.io/astral-sh/uv:latest /uv /usr/local/bin/uv

COPY pyproject.toml README.md ./
COPY sempkg_agent ./sempkg_agent
RUN uv pip install --system .

# Optionally bake the sempkg binary into the image:
# COPY sempkg /usr/local/bin/sempkg
# RUN chmod +x /usr/local/bin/sempkg

# Default workspace location (override SEMPKG_AGENT_MCP_WORKSPACE as needed).
ENV SEMPKG_AGENT_MCP_WORKSPACE=/workspace
WORKDIR /workspace

EXPOSE 8900
ENTRYPOINT ["sempkg-agent"]
CMD ["serve", "--transport", "a2a"]
