#!/usr/bin/env python3
import json
import sys


def main():
    marker = sys.argv[1] if len(sys.argv) > 1 else "request"
    message = json.load(sys.stdin)

    if message["phase"] == "request":
        request = message["request"]
        headers = request["headers"]
        headers.setdefault("x-traffwd-command-request", []).append(marker)
        print(json.dumps({"version": 1, "request": {"headers": headers}}))
        return

    response = message["response"]
    headers = response["headers"]
    headers.setdefault("x-traffwd-command-response", []).append(marker)
    print(json.dumps({"version": 1, "response": {"headers": headers}}))


if __name__ == "__main__":
    main()
