# Security

## Reporting a vulnerability

Report it privately at <https://github.com/erictran308/tuinews/security/advisories/new> (Security → Report a vulnerability), not in a public issue.

Say what a feed, an article's page or a picture has to contain, what happens, your tuinews version (`tuinews --version`), OS and terminal.

## What counts

Anything a feed, a page or a picture, or a file tuinews is pointed at, can do to you through tuinews. For example:

- a title, summary, article or picture that runs code, opens something without asking, or garbles the terminal;
- tuinews fetching from somewhere a feed shouldn't be able to send it: this computer, your network, plain http for pictures;
- anything sent anywhere that you didn't ask for;
- your feeds, what you read, or your history left where another account can read them.
