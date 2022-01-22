CATALOG TABLE emp ( "TYPE" = "CSV", "PATH" = "/Users/adarshrp/Projects/flare/data/emp.csv", "HEADER" = "YES", "SEPARATOR" = "," );

DESCRIBE TABLE emp;


select name, dept_id
from emp, emp
where emp.name = 'adarsh'
and age > 30
and dept_id > 1 + 2;
