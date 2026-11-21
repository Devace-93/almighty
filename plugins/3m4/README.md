# 3m4 agent-platform apps as service plugins

Manifests for flows, crew, sketch, blueprint and trace. Each app is its own
project and keeps running standalone; these files only tell almighty where
the app listens, which pages to embed and which resource links it opens.

```sh
almighty plugin add plugins/3m4/flows
```

The command prints an event secret once; give it to the app so both sides
can sign the events they exchange.

These manifests follow the platform's shared contract (ports, `/embed`
paths, `3m4://` resource types). They have not been run against the apps.
