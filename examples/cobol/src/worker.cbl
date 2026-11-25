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
FD  REQUESTS RECORD VARYING FROM 1 TO 2097152 DEPENDING ON IN-LEN.
01  REQUEST-LINE            PIC X(2097152).

WORKING-STORAGE SECTION.
COPY "message.cpy".
COPY "response.cpy".

01  IN-STATUS               PIC XX.
01  IN-LEN                  PIC 9(9) COMP-5.
01  FLUSH-RESULT            BINARY-LONG.
01  FOUND-TEXT              PIC X(128).
01  FOUND-LEN               BINARY-LONG.
01  REQUEST-ID              PIC X(128).
01  REQUEST-ID-LEN          BINARY-LONG.

PROCEDURE DIVISION.
MAIN-LOOP.
    OPEN INPUT REQUESTS
    DISPLAY '{"type":"ready","protocol":1}'
    PERFORM FLUSH-OUTPUT
    PERFORM READ-LINE
    PERFORM UNTIL IN-STATUS NOT = "00"
        CALL "JSON-FIND" USING MSG " " "type" FOUND-TEXT FOUND-LEN
        IF FOUND-TEXT = "request"
            PERFORM HANDLE-REQUEST
        END-IF
        PERFORM READ-LINE
    END-PERFORM
    *> Status 10 is the end of the input, which is how Unsworn stops it.
    IF IN-STATUS NOT = "10"
        DISPLAY "can't read a message: status " IN-STATUS UPON SYSERR
        STOP RUN RETURNING 1
    END-IF
    STOP RUN.

HANDLE-REQUEST.
    CALL "JSON-FIND" USING MSG " " "id" REQUEST-ID REQUEST-ID-LEN
    CALL "JSON-FIND" USING MSG " " "handler" FOUND-TEXT FOUND-LEN
    EVALUATE FOUND-TEXT
        WHEN "src/loans.cbl#payment"
            CALL "PAYMENT" USING MSG RESP
        WHEN "src/loans.cbl#schedule"
            CALL "SCHEDULE" USING MSG RESP
        WHEN "src/loans.cbl#afford"
            CALL "AFFORD" USING MSG RESP
        WHEN OTHER
            DISPLAY "no handler " FOUND-TEXT UPON SYSERR
            STOP RUN RETURNING 1
    END-EVALUATE
    DISPLAY '{"type":"response","id":"' REQUEST-ID(1:REQUEST-ID-LEN)
        '","status":200,"json":' RESP-BODY(1:RESP-LEN) '}'
    PERFORM FLUSH-OUTPUT.

*> A line that fills the record may have more to come, and the rest could be
*> read as a line of its own: part of one caller's request, taken for a
*> message. So the worker stops, as it does on any status but "00". (Some
*> versions of GnuCOBOL give a line that's too long "06", and some "00".)
READ-LINE.
    READ REQUESTS
    IF IN-STATUS = "00"
        IF IN-LEN >= LENGTH OF REQUEST-LINE
            MOVE "06" TO IN-STATUS
        ELSE
            MOVE IN-LEN TO MSG-LEN
            MOVE REQUEST-LINE(1:IN-LEN) TO MSG-TEXT(1:IN-LEN)
        END-IF
    END-IF.

FLUSH-OUTPUT.
    CALL "fflush" USING BY VALUE 0 RETURNING FLUSH-RESULT.

END PROGRAM WORKER.
