*> A Tier 1 worker: it reads requests as lines of JSON on stdin and
*> writes responses as lines of JSON on stdout. docs/protocol.md has the
*> protocol. Requests have no body here (see max_body in unsworn.toml): a
*> worker that takes them also has to read the raw bytes after a message
*> that has body_len.
IDENTIFICATION DIVISION.
PROGRAM-ID. WORKER.

ENVIRONMENT DIVISION.
INPUT-OUTPUT SECTION.
FILE-CONTROL.
    SELECT REQUESTS ASSIGN TO KEYBOARD
        ORGANIZATION LINE SEQUENTIAL
        FILE STATUS IN-STATUS.

DATA DIVISION.
FILE SECTION.
FD  REQUESTS RECORD VARYING FROM 1 TO 2097152 DEPENDING ON MSG-LEN.
01  REQUEST-LINE            PIC X(2097152).

WORKING-STORAGE SECTION.
COPY "message.cpy".
01  IN-STATUS               PIC XX.
01  MSG-TYPE                PIC X(16).
01  REQUEST-ID              PIC X(128).
01  HANDLER                 PIC X(128).
01  ANSWER                  PIC X(65536).
01  ANSWER-LEN              BINARY-LONG.

PROCEDURE DIVISION.
MAIN-LOOP.
    OPEN INPUT REQUESTS
    DISPLAY '{"type":"ready","protocol":1}'
    PERFORM FOREVER
        PERFORM READ-LINE
        *> Anything but a request, such as `cancel`, is safe to ignore for
        *> a worker that takes one request at a time.
        CALL "JSON-FIND" USING MSG " " "type" MSG-TYPE
        IF MSG-TYPE = "request"
            CALL "JSON-FIND" USING MSG " " "id" REQUEST-ID
            CALL "JSON-FIND" USING MSG " " "handler" HANDLER
            CALL "LOANS" USING HANDLER MSG ANSWER ANSWER-LEN
            DISPLAY '{"type":"response","id":"' FUNCTION TRIM(REQUEST-ID)
                '","status":200,"json":' ANSWER(1:ANSWER-LEN) '}'
        END-IF
    END-PERFORM.

*> Unsworn stops the worker by closing its input, which is status 10.
*>
*> A line that fills the record may have more to come, and the rest could be
*> read as a line of its own: part of one caller's request, taken for a
*> message. So the worker stops. (Some versions of GnuCOBOL give such a line
*> status 06, and some 00.)
READ-LINE.
    READ REQUESTS
    IF IN-STATUS = "10"
        STOP RUN
    END-IF
    IF IN-STATUS NOT = "00" OR MSG-LEN >= LENGTH OF REQUEST-LINE
        DISPLAY "a message is too long, or can't be read: status "
            IN-STATUS UPON SYSERR
        STOP RUN RETURNING 1
    END-IF
    MOVE REQUEST-LINE(1:MSG-LEN) TO MSG-TEXT(1:MSG-LEN).

END PROGRAM WORKER.
