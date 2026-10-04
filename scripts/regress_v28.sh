#!/bin/bash
# v2.8 regression driver: run every historical test suite, report tallies
source /home/z/my-project/scripts/env.sh
cd /home/z/glm-os/scripts
TOTAL=0; FAILED=0
for v in v12 v13 v16 v17 v18 v19 v20 v21 v22 v23 v24 v25 v26 v27; do
  echo "================ $v ================"
  python3 test_$v.py > /tmp/reg_$v.log 2>&1
  rc=$?
  tail_line=$(tail -3 /tmp/reg_$v.log | tr '\n' ' ')
  echo "rc=$rc  $tail_line"
  if [ $rc -ne 0 ]; then FAILED=$((FAILED+1)); echo "  ^^ FAILED — see /tmp/reg_$v.log"; fi
  TOTAL=$((TOTAL+1))
done
echo "================================"
echo "suites: $TOTAL, failed: $FAILED"
