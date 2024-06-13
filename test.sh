#!/bin/bash
curl -XPOST \
    -H "Content-Type: application/json" \
    -d@firehose.json \
    http://localhost:3000
