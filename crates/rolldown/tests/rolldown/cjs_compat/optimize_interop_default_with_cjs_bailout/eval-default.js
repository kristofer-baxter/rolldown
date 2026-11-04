let evaledDefaultValue = 1;
export default evaledDefaultValue;
eval('evaledDefaultValue = 2');
export const getEvaledDefaultValue = () => evaledDefaultValue;
