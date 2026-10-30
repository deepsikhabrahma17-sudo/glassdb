-- A short GlassDB demo. Run it with:
--   cargo run -p glassdb -- demo.db < examples/demo.sql

CREATE TABLE IF NOT EXISTS students (
  id INT PRIMARY KEY,
  name TEXT,
  grade REAL,
  enrolled BOOL
);

INSERT INTO students VALUES
  (1, 'Ada', 91.5, TRUE),
  (2, 'Bob', 72, FALSE),
  (3, 'Cy', NULL, TRUE);
INSERT INTO students (name, enrolled) VALUES ('Dee', TRUE);

SELECT * FROM students ORDER BY id;

SELECT name, grade + 5 AS curved
FROM students
WHERE enrolled AND grade IS NOT NULL
ORDER BY curved DESC;

UPDATE students SET grade = 88 WHERE name = 'Cy';
DELETE FROM students WHERE NOT enrolled;

SELECT id, name, grade FROM students ORDER BY grade DESC LIMIT 2;

.tables
.schema students
