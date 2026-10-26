# Cloudflare profile compatibility test

The ordinary test suite skips the live test. The live test creates a billable Browser Run session and navigates to `https://example.com/`. It uses a new mode-0700 temporary profile directory and synthetic cookies, localStorage, and IndexedDB data; it does not access the profiles under `CFB_HOME` or the default `~/.cf-browser` directory.

Install the workspace dependencies first. Configure `CLOUDFLARE_ACCOUNT_ID` and `CLOUDFLARE_API_TOKEN` in your environment, using a token with Browser Rendering Edit permission, then explicitly run:

```sh
AGENT_BROWSER_CLOUDFLARE_LIVE_TEST=1 pnpm --filter @agent-browser/cloudflare-profile test
```

The test checks that two independently authenticated CDP clients can use the same browser context and tab cookies, then saves and restores the isolated profile and verifies cookies, localStorage, and IndexedDB. It issues DELETE for the created session and polls for confirmation. If creation times out before an ID is returned, or deletion cannot be confirmed, check the Cloudflare dashboard and close any remaining test session. An abrupt process termination cannot guarantee cleanup; Cloudflare's inactivity timeout is the backstop.

A passing test does not enable shared writes in the native integration. Before enabling them, verify interoperable cross-process locking and crash-safe profile recovery with the installed Cloudflare plugin as well.
