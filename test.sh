export AUTH_SERVER="https://api.gingersociety.org/iam/docker-token"
export REGISTRY_HOST="docker.gingersociety.org"
export MY_TOKEN="eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJoZWxsb0BnaW5nZXJzb2NpZXR5Lm9yZyIsImV4cCI6MTc4MzA2ODAzNCwidXNlcl9pZCI6IjMiLCJ0b2tlbl90eXBlIjoiYWNjZXNzIiwiZmlyc3RfbmFtZSI6IkdpbmdlclNvY2lldHkiLCJsYXN0X25hbWUiOiJBZG1pbiIsIm1pZGRsZV9uYW1lIjpudWxsLCJjbGllbnRfaWQiOiJkZXYtcG9ydGFsLXN0YWdpbmcifQ.tKebsjXXjV0B-JPRth0U29UZy68rNiI6jzyAiqtjuYA"

curl -v -G "$AUTH_SERVER" \
  --data-urlencode "service=$REGISTRY_HOST" \
  --data-urlencode "scope=repository:rackmint/provisioner-service:pull" \
  --data-urlencode "account=__token__" \
  -u "__token__:$MY_TOKEN"