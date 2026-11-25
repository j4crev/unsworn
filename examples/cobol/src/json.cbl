*> JSON-FIND looks up one member of a message: KEY at the top level when
*> PARENT is a space, or KEY inside the top-level object PARENT.
*> FOUND-LEN is 0 if there's no such member, it's an object or an array,
*> or it doesn't fit FOUND-VALUE.
*>
*> It reads JSON as Unsworn writes it, with no spaces between tokens. That
*> isn't so of a request's JSON body, which is as the caller wrote it.
*>
*> It follows the nesting and compares whole names. Looking for the text
*> '"principal":' would be shorter, and would let a caller pick the value
*> by putting that text in a header.
IDENTIFICATION DIVISION.
PROGRAM-ID. JSON-FIND.

DATA DIVISION.
WORKING-STORAGE SECTION.
01  TEXT-POS                BINARY-LONG.
01  DEPTH                   BINARY-LONG.
01  TOKEN                   PIC X(256).
01  TOKEN-LEN               BINARY-LONG.
01  PARENT-STATE            PIC X.
    88  IN-PARENT           VALUE "Y".
    88  NOT-IN-PARENT       VALUE "N".
01  SEARCH-STATE            PIC X.
    88  SEARCHING           VALUE "N".
    88  SEARCH-DONE         VALUE "Y".

LINKAGE SECTION.
COPY "message.cpy".
01  PARENT                  PIC X ANY LENGTH.
01  KEY-NAME                PIC X ANY LENGTH.
01  FOUND-VALUE             PIC X ANY LENGTH.
01  FOUND-LEN               BINARY-LONG.

PROCEDURE DIVISION USING MSG PARENT KEY-NAME FOUND-VALUE FOUND-LEN.
FIND-MEMBER.
    MOVE SPACES TO FOUND-VALUE
    MOVE 0 TO FOUND-LEN
    MOVE 0 TO DEPTH
    SET NOT-IN-PARENT TO TRUE
    SET SEARCHING TO TRUE
    MOVE 1 TO TEXT-POS
    PERFORM UNTIL TEXT-POS > MSG-LEN OR SEARCH-DONE
        EVALUATE MSG-TEXT(TEXT-POS:1)
            WHEN "{"
            WHEN "["
                ADD 1 TO DEPTH
            WHEN "}"
            WHEN "]"
                SUBTRACT 1 FROM DEPTH
            WHEN '"'
                PERFORM READ-STRING
                IF DEPTH <= 2
                    PERFORM CHECK-NAME
                END-IF
        END-EVALUATE
        ADD 1 TO TEXT-POS
    END-PERFORM
    GOBACK.

*> Reads the string that starts at TEXT-POS into TOKEN, and leaves TEXT-POS
*> at its closing quote. TOKEN-LEN counts what doesn't fit TOKEN too.
*> Escapes stay as written, so a name with one never matches: "mo\nths"
*> must not become "months".
READ-STRING.
    MOVE 0 TO TOKEN-LEN
    ADD 1 TO TEXT-POS
    PERFORM UNTIL TEXT-POS > MSG-LEN OR MSG-TEXT(TEXT-POS:1) = '"'
        IF MSG-TEXT(TEXT-POS:1) = "\"
            PERFORM KEEP-CHARACTER
        END-IF
        PERFORM KEEP-CHARACTER
    END-PERFORM.

KEEP-CHARACTER.
    ADD 1 TO TOKEN-LEN
    IF TOKEN-LEN <= LENGTH OF TOKEN
        MOVE MSG-TEXT(TEXT-POS:1) TO TOKEN(TOKEN-LEN:1)
    END-IF
    ADD 1 TO TEXT-POS.

*> If TOKEN is a member's name, sees whether it's the one wanted. COBOL
*> pads the shorter side of a comparison with spaces, so the lengths are
*> compared too: "principal " isn't "principal".
CHECK-NAME.
    IF MSG-TEXT(TEXT-POS + 1:1) NOT = ":"
        EXIT PARAGRAPH
    END-IF

    IF DEPTH = 1
        SET NOT-IN-PARENT TO TRUE
        IF TOKEN-LEN = LENGTH OF PARENT
            IF TOKEN(1:TOKEN-LEN) = PARENT
                SET IN-PARENT TO TRUE
            END-IF
        END-IF
        IF PARENT NOT = SPACE
            EXIT PARAGRAPH
        END-IF
    ELSE
        IF NOT-IN-PARENT
            EXIT PARAGRAPH
        END-IF
    END-IF

    IF TOKEN-LEN = LENGTH OF KEY-NAME
        IF TOKEN(1:TOKEN-LEN) = KEY-NAME
            PERFORM READ-VALUE
            SET SEARCH-DONE TO TRUE
        END-IF
    END-IF.

READ-VALUE.
    ADD 2 TO TEXT-POS
    MOVE 0 TO TOKEN-LEN
    EVALUATE MSG-TEXT(TEXT-POS:1)
        WHEN '"'
            PERFORM READ-STRING
        WHEN "{"
        WHEN "["
            CONTINUE
        WHEN OTHER
            PERFORM KEEP-CHARACTER UNTIL TEXT-POS > MSG-LEN
                OR MSG-TEXT(TEXT-POS:1) = "," OR "}" OR "]"
    END-EVALUATE
    IF TOKEN-LEN > 0 AND TOKEN-LEN <= LENGTH OF FOUND-VALUE
            AND TOKEN-LEN <= LENGTH OF TOKEN
        MOVE TOKEN(1:TOKEN-LEN) TO FOUND-VALUE
        MOVE TOKEN-LEN TO FOUND-LEN
    END-IF.

END PROGRAM JSON-FIND.
