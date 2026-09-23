# Cookie Policy

**Last updated:** 2026-09-23

## What Cookies We Use

Every cookie below is set by the Service itself. The one exception is Paddle's checkout, described under [Paying for a plan](#paying-for-a-plan). None of them identify you to third parties, track your browsing across other sites, or facilitate advertising.

### Signing in

| Name | Purpose | Duration | Required? |
|---|---|---|---|
| `_sm_session` | Authenticate your session after sign-in | 90 days maximum (30 days idle) | Yes |
| `_sm_ml_confirm` | Ties the sign-in link you opened to the button you press on it, so nobody else can complete that sign-in | Lifetime of the link, 20 minutes by default | Only for email sign-in |
| `_sm_ml_code` | Binds the sign-in code we email you to the browser that asked for it, so the code is useless anywhere else | Lifetime of the link, 20 minutes by default | Only for email sign-in |

Without `_sm_session` you cannot stay signed in. The other two are set only while an email sign-in is in progress. `_sm_ml_confirm` is cleared as soon as the link is used, and `_sm_ml_code` once a code has been tried; either way both expire on their own shortly after the sign-in they belong to.

### Running the app

| Name | Purpose | Duration | Required? |
|---|---|---|---|
| `_sm_last_method` | Remembers which sign-in method you used last, so the sign-in page can point you back at it | 180 days | No |
| `_sm_flash` | Carries a one-off confirmation message across a redirect | 60 seconds | No |
| `_sm_deleted` | Shows the receipt for an account deletion you just requested | 5 minutes | No |

### Remembering your display choices

| Name | Purpose | Duration | Required? |
|---|---|---|---|
| `sm_theme` | Your light or dark choice, so the page paints in it before the stylesheet loads | 365 days | No |
| `sm_time_format` | Your 12-hour or 24-hour clock choice | 365 days | No |

These two mirror preferences already stored on your account. They are issued when you sign in and updated when you change the setting.

### Paying for a plan

When you open checkout to buy or manage a paid plan, the payment page loads Paddle.js from Paddle, our reseller. Paddle may set its own cookies or similar storage there to run the checkout and prevent fraud, as its privacy policy describes (<https://www.paddle.com/legal/privacy>). This happens only on the payment page, only after you choose to pay. Nothing from Paddle loads anywhere else in the Service.

## What Cookies We Don't Use

We do not use:

- Analytics cookies (our analytics is self-hosted and cookieless, so it sets none; see the Privacy Policy)
- Advertising cookies (no Google Ads, no Facebook Pixel, no DoubleClick)
- Third-party tracking cookies of any kind (Paddle's checkout cookies serve the payment you started, not tracking)
- Fingerprinting techniques as cookie alternatives

## Consent

Under the ePrivacy Directive and GDPR, cookies strictly necessary for a service you explicitly requested do not require consent, and neither do cookies that only remember a choice you made yourself. Every cookie above falls into one of those two groups: you receive them by signing in, by changing a setting, or by opening checkout to pay. We set no analytics, advertising or tracking cookie, so there is nothing to consent to and no banner to dismiss.

You can configure your browser to reject all cookies, but you would not be able to sign in.

## Public Status Pages

The public per-organisation status pages (e.g., `acme.example.com`)
do **not** set any cookies. They are fully anonymous.

## Contact

[hello@uptimepage.dev](mailto:hello@uptimepage.dev)
