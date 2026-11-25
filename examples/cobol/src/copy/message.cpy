*> One message from Unsworn: a line of JSON. The worker stops if one
*> doesn't fit (see READ-LINE).
01  MSG.
    05  MSG-LEN             BINARY-LONG.
    05  MSG-TEXT            PIC X(2097152).
