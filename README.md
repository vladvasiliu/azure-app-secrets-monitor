# Azure App Secrets Monitor
[![License](https://img.shields.io/github/license/vladvasiliu/azure-app-secrets-monitor)](LICENSE)

This program retrieves the secret key and certificate expiration dates from Azure AD App Registrations and exposes them
for Prometheus consumption.

## Project Status

Basic functionality is OK. Logging needs some improvement.

## Running

The default port is 9912.

It expects the following environment variables, which should be self-explanatory:

* `AASM_AZURE_TENANT_ID`
* `AASM_AZURE_CLIENT_ID`
* `AASM_AZURE_CLIENT_SECRET`
* `AASM_PORT` *(optional)*

Calling the `/metrics` endpoint returns the following metrics:

```openmetrics
# HELP scrape_status Whether the scrape was successful.
# TYPE scrape_status counter
scrape_status_total{outcome="success"} 5
scrape_status_total{outcome="failure"} 1
# HELP credential_expiration_time_seconds Timestamp of credential expiration.
# TYPE credential_expiration_time_seconds gauge
# UNIT credential_expiration_time_seconds seconds
credential_expiration_time_seconds{app_id="641cfdd2-e6e4-4bab-a64b-1f53733ffab0",app_name="My Super App",key_id="9cefcbbc-0644-4f34-9b82-01edd1ca3945"} 10413702000
credential_expiration_time_seconds{app_id="5ebf5719-b69c-4fb1-81ed-cff334dde909",app_name="Some other App",key_id="6cd0608d-da6b-4f46-8659-1a6d2bd39f82"} 10413702000
```

### Requirements

You need to register an AzureAD app for this exporter and add the `https://graph.microsoft.com/Application.Read.All` permission.

## License

This program is distributed under the terms of the [3-Clause BSD License](LICENSE).
