Agents waste their first step when a task or the model supplies a bare host such as `example.com`: the browser rejects it as an invalid URL, so the agent has to notice the error, add a scheme and retry.

Make every agent navigation path accept scheme-less targets by defaulting them to `https://`, after trimming surrounding whitespace. This covers the agent server's first navigation to a new session's start URL and every goto tool or action that the agents can execute, including the goto action handled by the shared CDP base agent and each agent's own goto handler. A bare host with a port or path, such as `localhost:3000` or `127.0.0.1:8080/health`, is scheme-less too. Leave targets that already carry a scheme unchanged, for example `http://…`, `https://…`, `file://…`, `chrome://…`, `about:blank` and `data:` URLs.

Add focused regression coverage.
