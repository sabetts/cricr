# bugs

* DONE add C-t M-t bindings
* DONE M-- M-u/M-l should not move the point
* DONE setting mark C-SPC doesn't work
* input line needs to wrap long lines.
* does C-c C-b reset the unread line?
* DONE movement keys like C-f/C-n/C-p/C-M-< should exit isearch
* pressing RET on an url should open it


* first connecting to entry needs a tiple * marker
* when typing a substring match and there become fewer than the original number of matches, dont shrink the echo area, pad it with empty lines.
* change buffers is C-x b
* match the colors to default rcirc. i've left a colors_list.txt file to help.
* DONE instead of hte rcirc text in the modeline show the server name
* DONE just the hour:minutes no brackets for timestamp
* DONE just show the timestamp when it's different from the prev line. show empty space otherwise.
* DONE for server messages prefix with *** not --> or >>> or -!- or it looks like sometimes its nothing
* DONE example bug

# features

* DONE C-c C-b opens query in the echo area to open the url. history is
  filled with urls from the buffer. when you select the url then it
  triggers opening it in the browser (when possible).
* DONE make buffer selection show a vertical list of up to 5 buffers in the
  echo area. you type and it does substring matching. ordering buffers
  based on which was most recently visited. C-n/C-p or up/down arrows
  highlights a buffer. RET selected it.
