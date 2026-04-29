---
icon: lucide/workflow
---

# Transform examples

Transforms sit between the source and sinks. Use them to set routing metadata, filter envelopes, mutate payloads, batch events, or run scripts.

## API to Kafka with a key

Use [`set_key`](../../configuration/transforms.md#set_key) to copy a payload field into `meta.key`, which the Kafka sink uses as the record key.

```toml
[[pipelines]]
name = "api->kafka-keyed"

[pipelines.source]
type = "api_poll"
url = "https://jsonplaceholder.typicode.com/posts/1"
interval_secs = 3

[[pipelines.transforms]]
type = "set_key"
from_field = "userId"

[[pipelines.sinks]]
type = "kafka"
brokers = "localhost:9092"
topic = "topic1"
```

## Rhai transform inline

Use [`script`](../../configuration/transforms.md#script) with Rhai for small, embedded transforms.

```toml
[[pipelines]]
name = "api->kafka-rhai"

[pipelines.source]
type = "api_poll"
url = "https://jsonplaceholder.typicode.com/posts/1"
interval_secs = 3

[[pipelines.transforms]]
type = "script"
runtime = "rhai"
on_error = "drop"
script = """
fn transform(env) {
  if env.payload["userId"] == 1 {
    env.meta.headers["priority"] = "high";
  }

  env.payload["processed"] = true;
  env
}
"""

[[pipelines.sinks]]
type = "kafka"
brokers = "localhost:9092"
topic = "topic1"
```

## Lua transform from a file

Keep longer scripts in version-controlled source files instead of inline TOML.

```toml
[[pipelines.transforms]]
type = "script"
runtime = "lua"
script_file = "./transforms/enrich.lua"
```

```lua title="transforms/enrich.lua"
function transform(env)
  env.payload.processed = true
  return env
end
```

## Python transform with a virtualenv

Point `python_bin` at a virtualenv if your script needs third-party packages.

```toml
[[pipelines.transforms]]
type = "script"
runtime = "python"
script_file = "./transforms/enrich.py"
python_bin = "./.venv/bin/python"
```

```python title="transforms/enrich.py"
import sys

def transform(env):
    print(f"processing key={env['meta']['key']}", file=sys.stderr)
    env["payload"]["processed"] = True
    return env
```
