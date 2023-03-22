#!/bin/sh

set -x
set -e

PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export PATH

tgt=$HOME/sjmb_slack/bin

rsync -var target/release/sjmb_slack $tgt/

exit 0
# EOF
