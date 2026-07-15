# Feishu Channel

Stellaclaw supports Feishu through the official Node.js SDK WebSocket long
connection mode. This does not require a public callback URL.

## Install bridge dependencies

```bash
npm install --prefix stellaclaw/sidecars/feishu-bridge
```

## Configure

Add a channel entry:

```json
{
  "kind": "feishu",
  "id": "feishu-main",
  "app_id_env": "FEISHU_APP_ID",
  "app_secret_env": "FEISHU_APP_SECRET",
  "domain": "feishu"
}
```

Then export credentials:

```bash
export FEISHU_APP_ID="cli_xxx"
export FEISHU_APP_SECRET="xxx"
```

Optional fields:

```json
{
  "encrypt_key_env": "FEISHU_ENCRYPT_KEY",
  "verification_token_env": "FEISHU_VERIFICATION_TOKEN",
  "allowed_chat_ids": ["oc_xxx"],
  "allowed_user_ids": ["ou_xxx"]
}
```

## Feishu Console

Use an enterprise self-built app. Enable bot capability, grant message receive
and send permissions, subscribe to `im.message.receive_v1`, and choose the
long-connection event subscription mode.

The first implementation supports text ingress, image/file attachment ingress,
and final assistant text replies. Incoming image and file resources are
downloaded by the sidecar and materialized under the target conversation's
`.stellaclaw/attachments/incoming/` directory as standard `FileItem` history
entries. Rich cards and threaded replies are not covered yet.
