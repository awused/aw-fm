#! /bin/sh
# Dumps all environment variables to a file

# Doesn't differentiate between set and unset values.

output="/tmp/aw-fm-env-vars"

echo AWFM_CURRENT_TAB_PATH=$AWFM_CURRENT_TAB_PATH > $output
echo AWFM_CURRENT_TAB_SEARCH=$AWFM_CURRENT_TAB_SEARCH >> $output
echo AWFM_NEXT_TAB_PATH=$AWFM_NEXT_TAB_PATH >> $output
echo AWFM_NEXT_TAB_SEARCH=$AWFM_NEXT_TAB_SEARCH >> $output
echo AWFM_PREV_TAB_PATH=$AWFM_PREV_TAB_PATH >> $output
echo AWFM_PREV_TAB_SEARCH=$AWFM_PREV_TAB_SEARCH >> $output
echo AWFM_SELECTION="$AWFM_SELECTION" >> $output

