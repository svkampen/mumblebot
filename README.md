# mumblebot

A Mumble sound bot.

# Set-up
Simply build using Cargo. Some native libraries may be required.

A configuration file and a certificate are required to run mumblebot.
To generate the certificate, run

```
openssl req -x509 -newkey rsa:4096 -keyout key.pem -out cert.pem -sha256 -days 3650 -nodes -subj "/CN=mumblebot"
```

The configuration file (`config.json`) should contain the following schema:

```
{
    "host": "example.org",
    "port": 64738,
    "username": "Mumblebot",
    # for spotify search
    "rspotify_client_id": "<id>",
    "rspotify_client_secret": "<secret>"
}
```
