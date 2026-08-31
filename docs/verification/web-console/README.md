# Web console — browser verification

Driven against the shipped path: the Rust server serving the built SPA at `/console`
(`OG_WEB_CONSOLE_DIR=web/dist`), a real Chrome via CDP, seed org `Acme` with an admin
(`admin@acme.test`) and a member (`mel@acme.test`). Every flow was actually driven, not asserted
in the abstract; the screenshots are the record.

| # | Flow | Screenshot |
|---|------|-----------|
| 1 | Sign in (dark Open Grok brand, same shell as the auth pages) | `01-login.jpg` |
| 2 | Edit name → save → persists on reload | `02-account-name-saved.jpg` |
| 3 | **Member** account: the **Admin tab is hidden** | `03-member-no-admin-tab.jpg` |
| 4 | **Member** typing `/console/admin` is **redirected to `/account`** | `04-member-admin-redirected.jpg` |
| 5 | **Admin** account: the **Admin tab appears** | `05-admin-has-admin-tab.jpg` |
| 6 | Admin dashboard: org users listed (admin + member), enabled | `06-admin-users-list.jpg` |
| 7 | Admin disables the member → state flips (and back) | `07-admin-disabled-member.jpg` |
| 8 | Admin issues an invite → code + copyable signup link, `open` | `08-admin-issue-invite.jpg` |
| 9 | Avatar: pick image → data-URL preview → save → persists | `09-avatar-preview.jpg` |
| 10 | Change password → success | `10-password-changed.jpg` |
| 11 | Sign out → back to the login page | `11-signed-out.jpg` |

Password change was also proven end-to-end: changed in the browser, signed out, signed back in
with the **new** password.

## Two bugs found by this verification, both fixed here

1. **Admin surface shown to members.** `/account` did not report whether the caller is their org's
   admin, so the Admin tab showed for everyone and `/console/admin` was reachable (it only fell
   back to a server 403). Fix: `/account` returns `isAdmin`; the tab is hidden and the route
   redirects for non-admins; the API still enforces `admin` server-side (two layers).

2. **Login clobbered the account projection.** `mint_session`/`rotate` wrote a bare `session_only`
   view, which `append_account` upserts over `account_view` — wiping the person's name and
   `enabled` flag on every sign-in. Invisible to `GET /account` (it reads the event-sourced
   aggregate) but corrupting to the admin user list (which reads the projection). Fix: both paths
   now write the account's real state. Guarded by
   `logging_in_does_not_clobber_the_account_projection` and by slice19's step 8.

Also added: the admin user list is now org-scoped (`accounts_by_org`) so CLI-created accounts and
the admin appear, not only invite redeemers; and an admin cannot disable their own account (409),
which would risk locking the org out.
