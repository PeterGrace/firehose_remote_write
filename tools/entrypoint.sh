#!/bin/bash

cd /opt/app

if [[ ! -z "$DEBUG" ]]
then
    echo "Debug set -- sleeping infinity for exec purposes"
    sleep infinity

fi
/opt/app/firehose_remote_write
