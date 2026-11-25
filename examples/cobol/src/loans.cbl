*> The handlers, a paragraph each. A handler reads its inputs from the
*> message's query, which Unsworn has checked against the route's rules,
*> and writes its answer as JSON.
IDENTIFICATION DIVISION.
PROGRAM-ID. LOANS.

DATA DIVISION.
WORKING-STORAGE SECTION.
*> A loan, and where its repayment has got to.
01  LOAN-PRINCIPAL          PIC 9(11)V99.
01  LOAN-RATE               PIC 9(2)V9(4).
01  LOAN-MONTHS             PIC 9(3).
01  LOAN-EXACT              PIC 9(11)V9(10).
01  LOAN-PAYMENT            PIC 9(11)V99.
01  LOAN-MONTH              PIC 9(3).
01  LOAN-BALANCE            PIC 9(11)V99.
01  MONTH-PAYMENT           PIC 9(11)V99.
01  MONTH-PRINCIPAL         PIC 9(11)V99.
01  MONTH-INTEREST          PIC 9(11)V99.
01  TOTAL-PAID              PIC 9(13)V99.
01  TOTAL-INTEREST          PIC 9(13)V99.

*> Numbers as JSON has them, once the spaces before them are trimmed.
01  EDIT-MONTH              PIC ZZ9.
01  EDIT-PAYMENT            PIC Z(12)9.99.
01  EDIT-PRINCIPAL          PIC Z(12)9.99.
01  EDIT-INTEREST           PIC Z(12)9.99.
01  EDIT-BALANCE            PIC Z(12)9.99.
01  EDIT-LAST               PIC Z(12)9.99.
01  EDIT-PAID               PIC Z(12)9.99.

01  QUERY-VALUE             PIC X(16).

LINKAGE SECTION.
01  HANDLER                 PIC X ANY LENGTH.
COPY "message.cpy".
01  ANSWER                  PIC X ANY LENGTH.
01  ANSWER-LEN              BINARY-LONG.

PROCEDURE DIVISION USING HANDLER MSG ANSWER ANSWER-LEN.
CALL-HANDLER.
    *> STRING moves ANSWER-LEN along as it writes, to one past the end.
    MOVE 1 TO ANSWER-LEN
    EVALUATE HANDLER
        WHEN "src/loans.cbl#payment"
            PERFORM PAYMENT
        WHEN "src/loans.cbl#schedule"
            PERFORM SCHEDULE
        WHEN "src/loans.cbl#afford"
            PERFORM AFFORD
        WHEN OTHER
            DISPLAY "no handler " HANDLER UPON SYSERR
            STOP RUN RETURNING 1
    END-EVALUATE
    SUBTRACT 1 FROM ANSWER-LEN
    GOBACK.

*> GET /loans/payment: what a loan costs.
PAYMENT.
    PERFORM START-LOAN
    *> Added up month by month, because each is rounded to the cent.
    MOVE 0 TO TOTAL-PAID TOTAL-INTEREST
    PERFORM UNTIL LOAN-BALANCE = 0
        PERFORM PAY-MONTH
        ADD MONTH-PAYMENT TO TOTAL-PAID
        ADD MONTH-INTEREST TO TOTAL-INTEREST
    END-PERFORM

    MOVE LOAN-PAYMENT TO EDIT-PAYMENT
    MOVE MONTH-PAYMENT TO EDIT-LAST
    MOVE LOAN-MONTH TO EDIT-MONTH
    MOVE TOTAL-PAID TO EDIT-PAID
    MOVE TOTAL-INTEREST TO EDIT-INTEREST
    STRING '{"payment":' FUNCTION TRIM(EDIT-PAYMENT)
        ',"last_payment":' FUNCTION TRIM(EDIT-LAST)
        ',"payments":' FUNCTION TRIM(EDIT-MONTH)
        ',"total_paid":' FUNCTION TRIM(EDIT-PAID)
        ',"total_interest":' FUNCTION TRIM(EDIT-INTEREST) '}'
        DELIMITED BY SIZE INTO ANSWER WITH POINTER ANSWER-LEN.

*> GET /loans/schedule: every payment, and what it goes towards.
SCHEDULE.
    PERFORM START-LOAN
    STRING '{"months":[' DELIMITED BY SIZE
        INTO ANSWER WITH POINTER ANSWER-LEN
    PERFORM UNTIL LOAN-BALANCE = 0
        PERFORM PAY-MONTH
        MOVE LOAN-MONTH TO EDIT-MONTH
        MOVE MONTH-PAYMENT TO EDIT-PAYMENT
        MOVE MONTH-PRINCIPAL TO EDIT-PRINCIPAL
        MOVE MONTH-INTEREST TO EDIT-INTEREST
        MOVE LOAN-BALANCE TO EDIT-BALANCE
        IF LOAN-MONTH > 1
            STRING "," DELIMITED BY SIZE
                INTO ANSWER WITH POINTER ANSWER-LEN
        END-IF
        STRING '{"month":' FUNCTION TRIM(EDIT-MONTH)
            ',"payment":' FUNCTION TRIM(EDIT-PAYMENT)
            ',"principal":' FUNCTION TRIM(EDIT-PRINCIPAL)
            ',"interest":' FUNCTION TRIM(EDIT-INTEREST)
            ',"balance":' FUNCTION TRIM(EDIT-BALANCE) '}'
            DELIMITED BY SIZE INTO ANSWER WITH POINTER ANSWER-LEN
    END-PERFORM
    STRING "]}" DELIMITED BY SIZE
        INTO ANSWER WITH POINTER ANSWER-LEN.

*> GET /loans/afford: the largest loan a monthly payment pays off.
AFFORD.
    CALL "JSON-FIND" USING MSG "query" "payment" QUERY-VALUE
    PERFORM CHECK-NUMBER
    COMPUTE LOAN-PAYMENT = FUNCTION NUMVAL(QUERY-VALUE)
    PERFORM READ-TERMS

    *> Rounded down: a cent more would take a larger payment.
    COMPUTE LOAN-EXACT ROUNDED = LOAN-PAYMENT
        / FUNCTION ANNUITY(LOAN-RATE / 1200, LOAN-MONTHS)
    MOVE LOAN-EXACT TO LOAN-PRINCIPAL
    MOVE LOAN-PRINCIPAL TO EDIT-PRINCIPAL
    STRING '{"principal":' FUNCTION TRIM(EDIT-PRINCIPAL) '}'
        DELIMITED BY SIZE INTO ANSWER WITH POINTER ANSWER-LEN.

*> Reads a loan from the query and works out its monthly payment. COBOL
*> has a function for that: ANNUITY is the payment on a loan of 1. Its digits
*> are cut off somewhere (ANNUITY(0, 12) is 0.08333...), which would turn a
*> payment of exactly half a cent into slightly less than half. LOAN-EXACT
*> puts that right before anything is rounded to the cent.
START-LOAN.
    CALL "JSON-FIND" USING MSG "query" "principal" QUERY-VALUE
    PERFORM CHECK-NUMBER
    COMPUTE LOAN-PRINCIPAL = FUNCTION NUMVAL(QUERY-VALUE)
    PERFORM READ-TERMS
    COMPUTE LOAN-EXACT ROUNDED = LOAN-PRINCIPAL
        * FUNCTION ANNUITY(LOAN-RATE / 1200, LOAN-MONTHS)
    COMPUTE LOAN-PAYMENT ROUNDED = LOAN-EXACT
    MOVE LOAN-PRINCIPAL TO LOAN-BALANCE
    MOVE 0 TO LOAN-MONTH.

*> Pays one month's interest and some of the balance. The last payment is
*> whatever is left. It can come early: a payment rounded up to the cent
*> can pay off a small loan ahead of time.
PAY-MONTH.
    ADD 1 TO LOAN-MONTH
    COMPUTE MONTH-INTEREST ROUNDED = LOAN-BALANCE * LOAN-RATE / 1200
    COMPUTE MONTH-PAYMENT = FUNCTION MIN(LOAN-PAYMENT,
        LOAN-BALANCE + MONTH-INTEREST)
    IF LOAN-MONTH = LOAN-MONTHS
        ADD LOAN-BALANCE MONTH-INTEREST GIVING MONTH-PAYMENT
    END-IF
    SUBTRACT MONTH-INTEREST FROM MONTH-PAYMENT GIVING MONTH-PRINCIPAL
    SUBTRACT MONTH-PRINCIPAL FROM LOAN-BALANCE.

READ-TERMS.
    CALL "JSON-FIND" USING MSG "query" "rate" QUERY-VALUE
    PERFORM CHECK-NUMBER
    COMPUTE LOAN-RATE = FUNCTION NUMVAL(QUERY-VALUE)
    CALL "JSON-FIND" USING MSG "query" "months" QUERY-VALUE
    PERFORM CHECK-NUMBER
    COMPUTE LOAN-MONTHS = FUNCTION NUMVAL(QUERY-VALUE)
    IF LOAN-MONTHS = 0
        DISPLAY "a loan has no months" UPON SYSERR
        STOP RUN RETURNING 1
    END-IF.

*> The routes require every number a handler reads. If one stops doing so,
*> the worker stops too.
CHECK-NUMBER.
    IF FUNCTION TEST-NUMVAL(QUERY-VALUE) NOT = 0
        DISPLAY "the query isn't a loan" UPON SYSERR
        STOP RUN RETURNING 1
    END-IF.

END PROGRAM LOANS.
