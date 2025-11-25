A basic http-connect proxy

To test, start the proxy:
```shell
cargo run
```

then configure your app to use it, eg:
```shell
curl -v -x http://127.0.0.1:8080 https://www.google.com
```