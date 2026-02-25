;;; Remora Lisp standard macros — embedded at compile time, evaluated at startup.
;;;
;;; These macros are available in every .reml file without any import.

;; (define-service var-name "svc-name"
;;   (:image    "img")
;;   (:network  "net")
;;   (:port     host-var container)
;;   (:memory   mem-var)
;;   ...)
;;
;; Each option is a list (:keyword args...).  The macro strips the leading ':'
;; from the keyword symbol and generates (list 'keyword args...) for each
;; option, splicing them all into a (service ...) call bound to var-name.
;;
;; Variables in value positions are NOT quoted — they are evaluated at call-site:
;;   (:port jupyter-port 8888)   ; jupyter-port is looked up in caller's env
;;   (:memory mem-redis)         ; mem-redis is looked up in caller's env
;;   (:image "alpine:latest")    ; string literal, no variable needed
(defmacro define-service (var-name svc-name . opts)
  (define (expand-opt opt)
    (let* ((kw   (car opt))
           (sym  (string->symbol (substring (symbol->string kw) 1)))
           (args (cdr opt)))
      `(list ',sym ,@args)))
  `(define ,var-name
     (service ,svc-name ,@(map expand-opt opts))))
